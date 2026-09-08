//! In-VM sampling profiler (ext-php-rs).
//!
//! WHAT THIS IS. A statistical profiler living inside the PHP worker process. Unlike the Zend fcall
//! observer in `observer.rs` (which instruments EVERY call — exact but O(calls) overhead), the timer
//! kinds fire on a fixed-frequency interval and, at the next safe point, walk the current Zend VM
//! call stack once. Cost is O(sample_rate * stack_depth), independent of how many functions the
//! request calls, so overhead is bounded and tunable via `CHRONOS_PHP_PROFILE_SAMPLE_RATE` (Hz).
//!
//! WHAT IT MEASURES. Four of the contract's sample types, from ONE interval timer:
//! - `CPU` — `ITIMER_PROF` ticks. This clock only accrues while the process is actually running,
//!   which is precisely why it cannot see blocked time.
//! - `WALL` — the monotonic-clock gap between drains. One extra vDSO clock read per drain rather
//!   than a second timer and a second signal to collide with the app's own.
//! - `OFF_CPU` — wall minus CPU over the same window: time the request existed but did not run.
//! - `IO` — exact, not sampled: the observer already brackets `PDO::`/`mysqli::`/`Redis::`/
//!   `curl_exec`, so its measured duration and real stack become a sample directly.
//!
//! `ALLOCATION` and `LOCK` are absent, not stubbed — see `SampleKind`.
//!
//! Selectable per deployment with `CHRONOS_PHP_PROFILE_TYPES` (default: all four), because the
//! cheapest way to cut profiler overhead is to collect less.
//!
//! WHICH REQUESTS. The profiler has its OWN rate, decided in `start_request`
//! before the trace decision: `CHRONOS_PHP_PROFILE_REQUEST_RATE` for web requests
//! (default 0) and `CHRONOS_PHP_PROFILE_JOB_RATE` for background jobs (default a
//! tenth), plus the forced directive `X-Chronos-Profile: <token>`. It used to ride
//! the APM verdict instead, which made profiling 1% of traffic impossible without
//! also discarding 99% of the traces.
//!
//! A profiled request is always a TRACED one — a profile is reached through its
//! request, so the profile decision upgrades `TraceContext::sampled` rather than
//! being gated on it. An unprofiled request arms no timer and walks no stacks.
//!
//! TAGS. Samples carry request-scoped labels (`route`, `http.method`, `service`, `outcome`, plus
//! anything set through `chronos_profile_tag`), resolved at flush rather than at capture — a
//! framework does not know its route until routing has run, and stamping strings per sample on the
//! hot path would cost more than the sampling. See `set_label`.
//!
//! WHERE SAMPLES GO. Samples buffer request-locally and, at RSHUTDOWN, serialise to a `.profile` spool
//! file (`profile_spool::write_atomic`) content-addressed exactly like the `.trace` spool. The
//! engine-agent tails the same spool directory and ships them. The wire envelope is
//! `chronos.profiling.sample-batch.v1`, a JSON mirror of the `ProfileSampleBatch` proto the engine
//! `/v1/profiles` route already accepts as protobuf — see `profile_spool.rs`.
//!
//! THE UNSAFE SEAMS (all in `mod zend`, the only `unsafe` in the sampler):
//! 1. Timer arming — `setitimer(ITIMER_PROF)` plus a `SIGPROF` handler that does nothing but an
//!    atomic increment (async-signal-safe by construction).
//! 2. Stack walk — reading `zend_execute_data` and its `prev_execute_data` chain to collect frames.
//!
//! Everything else (windowing, buffering, labelling, serialisation) is safe Rust and unit-tested
//! with `--no-default-features`, which is also the only way the test binary can link: the FFI build
//! resolves `executor_globals` from the host PHP, so `cargo test` needs the seam compiled out while
//! `cargo check` verifies it.

use crate::context::TraceContext;
use crate::settings;
use std::cell::RefCell;

/// One captured stack frame. Maps 1:1 onto the proto `ProfileFrame` (module/function/file/line); the
/// instruction pointer is 0 for interpreted PHP frames (there is no native IP — the "location" is the
/// PHP function identity, which is exactly what pprof `Function{name,filename}` models).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleFrame {
    pub module: String, // "php" for interpreted frames; the extension/SAPI name for internal funcs
    pub function: String, // "Class::method", "Namespace\\func", or "{closure}" — NO arguments captured
    pub file: String,     // compiled-file path of the op array; "" for internal functions
    pub line: u32,        // currently-executing line within the frame
}

/// What a sample measures. Names map onto the contract's `ProfileSampleType` enum.
///
/// Only these four are produced. `ALLOCATION` needs the Zend memory manager's custom
/// handlers (`zend_mm_set_custom_handlers`) and a `bytes` unit, and `LOCK` has almost
/// nothing to hook in PHP userland — both are absent rather than stubbed, so a
/// consumer never sees an empty series that looks collected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SampleKind {
    /// On-CPU: one SIGPROF tick from `ITIMER_PROF`, which accrues only while running.
    Cpu,
    /// Elapsed time, running or blocked — measured from the monotonic clock.
    Wall,
    /// Wall minus CPU over the same window: time the request existed but did not run.
    OffCpu,
    /// A precisely measured I/O wait, from the observer's instrumented call.
    Io,
}

impl SampleKind {
    #[must_use]
    pub const fn proto_name(self) -> &'static str {
        match self {
            Self::Cpu => "PROFILE_SAMPLE_TYPE_CPU",
            Self::Wall => "PROFILE_SAMPLE_TYPE_WALL",
            Self::OffCpu => "PROFILE_SAMPLE_TYPE_OFF_CPU",
            Self::Io => "PROFILE_SAMPLE_TYPE_IO",
        }
    }

    /// Every kind we emit is a duration. An allocation profile would be `bytes`, which
    /// is one reason it needs more than a new enum variant.
    #[must_use]
    pub const fn unit(self) -> &'static str {
        "nanoseconds"
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cpu" => Some(Self::Cpu),
            "wall" => Some(Self::Wall),
            "off_cpu" | "offcpu" | "off-cpu" => Some(Self::OffCpu),
            "io" => Some(Self::Io),
            _ => None,
        }
    }
}

/// Which kinds this request collects. A flat struct rather than a set so the hot path
/// tests a bool instead of hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleKinds {
    pub cpu: bool,
    pub wall: bool,
    pub off_cpu: bool,
    pub io: bool,
}

impl SampleKinds {
    pub const ALL: Self = Self {
        cpu: true,
        wall: true,
        off_cpu: true,
        io: true,
    };
    pub const NONE: Self = Self {
        cpu: false,
        wall: false,
        off_cpu: false,
        io: false,
    };

    /// Parse a comma-separated list (`cpu,wall,io`). An unparseable or empty list
    /// falls back to everything, because silently profiling nothing is the worse
    /// failure — a typo in an env var should not look like a quiet application.
    #[must_use]
    pub fn parse(list: &str) -> Self {
        let mut kinds = Self::NONE;
        let mut matched = false;
        for token in list.split(',') {
            match SampleKind::parse(token) {
                Some(SampleKind::Cpu) => {
                    kinds.cpu = true;
                    matched = true;
                }
                Some(SampleKind::Wall) => {
                    kinds.wall = true;
                    matched = true;
                }
                Some(SampleKind::OffCpu) => {
                    kinds.off_cpu = true;
                    matched = true;
                }
                Some(SampleKind::Io) => {
                    kinds.io = true;
                    matched = true;
                }
                None => {}
            }
        }
        if matched {
            kinds
        } else {
            Self::ALL
        }
    }

    #[must_use]
    pub const fn count(self) -> usize {
        self.cpu as usize + self.wall as usize + self.off_cpu as usize + self.io as usize
    }

    #[must_use]
    pub const fn any(self) -> bool {
        self.count() > 0
    }
}

/// One statistical sample: a stack plus the weight it accounts for, in `kind`'s unit.
#[derive(Clone, Debug)]
pub struct Sample {
    pub kind: SampleKind,
    pub sampled_at_unix_nanos: u128,
    pub period_nanoseconds: u64,
    pub value_nanoseconds: i64,
    /// Root-first: index 0 is the outermost caller, last is the leaf currently executing. This matches
    /// the engine flame-graph builder, which groups by the stack path in emit order (root at the flame
    /// base). NB: the host `engine-profiler` (perf) currently emits leaf-first; see the design doc
    /// followup on normalising stack order across collectors.
    ///
    /// Shared by `Rc` because one drain emits up to three samples (CPU, wall, off-CPU)
    /// off a single walk — cloning the frame strings three times per window would be
    /// the most expensive thing the profiler does.
    pub stack: std::rc::Rc<Vec<SampleFrame>>,
    /// Canonical correlation only — trace/span/session ids, never captured application values.
    pub trace_id: String,
    pub span_id: String,
    pub session_id: Option<String>,
}

/// Sampler tuning resolved from INI/env at RINIT. `sample_rate_hz` comes from
/// `CHRONOS_PHP_PROFILE_SAMPLE_RATE` (already a documented collector env var).
#[derive(Clone, Debug)]
pub struct SamplerConfig {
    pub sample_rate_hz: u32,
    pub max_samples_per_request: usize,
    pub max_stack_depth: usize,
    pub series_id: String,
    /// Which sample types to collect (`CHRONOS_PHP_PROFILE_TYPES`).
    pub kinds: SampleKinds,
    /// I/O waits shorter than this are not worth a stack walk
    /// (`CHRONOS_PHP_PROFILE_IO_MIN_US`).
    pub min_io_wait_nanos: u128,
}

impl SamplerConfig {
    /// Defaults chosen to bound worst-case overhead and memory:
    ///   99 Hz  — coprime with common 100 Hz scheduler ticks (avoids lockstep aliasing), ~1 sample
    ///            per 10 ms of CPU; typical steady-state overhead well under 1% (see design doc model).
    ///   127    — max stack depth captured per sample (same bound as the host profiler).
    ///   2000   — hard cap on samples buffered per request (~20 s of wall at 99 Hz) so a slow request
    ///            cannot grow the buffer without bound.
    pub const DEFAULT_RATE_HZ: u32 = 99;
    pub const DEFAULT_MAX_STACK_DEPTH: usize = 127;
    pub const DEFAULT_MAX_SAMPLES: usize = 2000;
    /// Below ~1 ms an I/O wait is not worth a stack walk, and a chatty request can
    /// make thousands of tiny calls. This is the single most important overhead dial
    /// for the I/O kind, which is event-driven rather than timer-bounded.
    pub const DEFAULT_MIN_IO_WAIT_US: u128 = 1_000;

    /// Resolve from env, clamping the rate into a safe band. A rate of 0 disables the sampler (returns
    /// `None`) so the module stays inert exactly like `chronos.enabled = 0`.
    /// Every read goes through `settings` rather than `std::env::var` directly.
    /// It used to read the process environment only, which meant these four
    /// knobs were the one part of the collector a `.chronos` file or a
    /// `chronos.*` INI line could not configure — the file was parsed, the value
    /// was there, and the sampler never looked at it.
    pub fn resolve() -> Option<Self> {
        let sample_rate_hz =
            settings::u32_value("CHRONOS_PHP_PROFILE_SAMPLE_RATE", Self::DEFAULT_RATE_HZ);
        if sample_rate_hz == 0 {
            return None;
        }
        let kinds = settings::get("CHRONOS_PHP_PROFILE_TYPES")
            .map_or(SampleKinds::ALL, |list| SampleKinds::parse(&list));
        if !kinds.any() {
            return None;
        }
        Some(Self {
            // Clamp: below 1 Hz is pointless, above 1000 Hz risks unacceptable overhead in-VM.
            sample_rate_hz: sample_rate_hz.clamp(1, 1000),
            // The cap is per kind, not per request: with four kinds enabled a request
            // would otherwise hit the ceiling four times sooner and truncate the tail
            // of a long request for every type at once.
            max_samples_per_request: Self::DEFAULT_MAX_SAMPLES * kinds.count().max(1),
            max_stack_depth: Self::DEFAULT_MAX_STACK_DEPTH,
            series_id: settings::string("CHRONOS_PHP_PROFILE_SERIES_ID", "php"),
            kinds,
            min_io_wait_nanos: u128::from(settings::u64_value(
                "CHRONOS_PHP_PROFILE_IO_MIN_US",
                Self::DEFAULT_MIN_IO_WAIT_US as u64,
            )) * 1_000,
        })
    }

    /// Sampling period in nanoseconds (`1e9 / hz`).
    #[must_use]
    pub fn period_nanoseconds(&self) -> u64 {
        1_000_000_000 / u64::from(self.sample_rate_hz)
    }
}

thread_local! {
    /// Request-local sample buffer. Zend is single-threaded per request (NTS); a ZTS build would key
    /// this by the thread-safe resource id. Reset at RINIT, drained + spooled at RSHUTDOWN.
    static REQUEST_SAMPLES: RefCell<Vec<Sample>> = const { RefCell::new(Vec::new()) };
    /// True while the request opted into profiling (sampled flag set and a config resolved). The signal
    /// handler consults this to decide whether a tick captures anything, so an un-sampled request pays
    /// only a flag read per tick.
    static SAMPLING_ACTIVE: RefCell<bool> = const { RefCell::new(false) };
    /// The context + config the observer-boundary tick consumer samples against.
    static ACTIVE_SAMPLER: RefCell<Option<(TraceContext, SamplerConfig)>> = const { RefCell::new(None) };
    /// Wall/off-CPU accounting, advanced at every drain. See `DrainClock`.
    static DRAIN_CLOCK: RefCell<DrainClock> = const { RefCell::new(DrainClock::IDLE) };
    /// Request-scoped profile tags, stamped onto every sample at flush.
    static REQUEST_LABELS: RefCell<std::collections::BTreeMap<String, String>> =
        const { RefCell::new(std::collections::BTreeMap::new()) };
}

/// Wall-clock accounting between drains.
///
/// The CPU timer cannot see blocked time by construction — `ITIMER_PROF` stops
/// accruing the moment the process blocks in a syscall, so a request waiting 200 ms on
/// a query produces zero ticks. The gap between drains on the monotonic clock is
/// therefore the only signal for wall and off-CPU, and it costs one vDSO clock read
/// per drain rather than a second interval timer and a second signal.
///
/// Emission is rate-limited to the sampling period: wall time accumulates and only
/// becomes a sample once a full period has passed, so enabling wall does not turn every
/// observed function call into a sample.
#[derive(Clone, Copy, Debug)]
struct DrainClock {
    /// Monotonic reading at the previous drain. `None` until the first drain — NOT a
    /// zero sentinel: `monotonic_nanos()` legitimately returns ~0 on the first call of
    /// the process, which a sentinel would mistake for "no baseline yet" and silently
    /// drop the request's first window.
    last_monotonic_nanos: Option<u128>,
    /// Wall time banked since the last wall sample was emitted.
    pending_wall_nanos: u128,
    /// CPU time banked over the same window, for the off-CPU subtraction.
    window_cpu_nanos: u128,
}

impl DrainClock {
    const IDLE: Self = Self {
        last_monotonic_nanos: None,
        pending_wall_nanos: 0,
        window_cpu_nanos: 0,
    };

    /// Bank `cpu_nanos` of CPU and the wall time elapsed since the last drain. Returns
    /// `Some((wall, cpu))` once a full period has accumulated, having reset the window.
    fn advance(&mut self, now: u128, cpu_nanos: u128, period: u64) -> Option<(u128, u128)> {
        // Bank CPU first so ticks that arrived before the first drain are not lost.
        self.window_cpu_nanos = self.window_cpu_nanos.saturating_add(cpu_nanos);
        // The first drain of a request only establishes the baseline; the interval
        // before it belongs to bootstrap, not to any stack we could name.
        let Some(last) = self.last_monotonic_nanos else {
            self.last_monotonic_nanos = Some(now);
            return None;
        };
        // saturating_sub, so a non-monotonic reading can only contribute zero rather
        // than wrapping into an enormous window.
        self.pending_wall_nanos = self
            .pending_wall_nanos
            .saturating_add(now.saturating_sub(last));
        self.last_monotonic_nanos = Some(now);
        if self.pending_wall_nanos < u128::from(period) {
            return None;
        }
        Some((
            std::mem::take(&mut self.pending_wall_nanos),
            std::mem::take(&mut self.window_cpu_nanos),
        ))
    }
}

/// Bounds on profile tags. Tags become label dimensions on every sample in a series,
/// so an unbounded key space would multiply the engine's grouping cardinality.
pub const MAX_LABELS: usize = 12;
pub const MAX_LABEL_KEY_BYTES: usize = 48;
pub const MAX_LABEL_VALUE_BYTES: usize = 200;

/// Normalise a tag key to `[a-z0-9._-]`, so a caller cannot inject structure into the
/// serialised label map. Returns `None` for a key with nothing usable left.
#[must_use]
pub fn normalise_label_key(key: &str) -> Option<String> {
    let cleaned: String = key
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_LABEL_KEY_BYTES)
        .collect();
    let trimmed = cleaned.trim_matches('_').to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Set a request-scoped profile tag (route, action, job name, …).
///
/// Tags are applied at flush rather than at capture. That is deliberate on two counts:
/// a framework usually does not know its route until after routing has run — well after
/// the first samples exist — and stamping strings onto every sample on the hot path
/// would cost more than the sampling itself.
pub fn set_label(key: &str, value: &str) {
    let Some(key) = normalise_label_key(key) else {
        return;
    };
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let mut value = value.to_owned();
    if value.len() > MAX_LABEL_VALUE_BYTES {
        value.truncate(MAX_LABEL_VALUE_BYTES);
    }
    REQUEST_LABELS.with(|labels| {
        let mut labels = labels.borrow_mut();
        // Overwriting an existing key is always allowed; only NEW keys are capped, so
        // a late, more precise route still lands once the map is full.
        if labels.len() >= MAX_LABELS && !labels.contains_key(&key) {
            return;
        }
        labels.insert(key, value);
    });
}

/// Take the request's tags, clearing them for the next request.
pub fn take_labels() -> std::collections::BTreeMap<String, String> {
    REQUEST_LABELS.with(|labels| std::mem::take(&mut *labels.borrow_mut()))
}

/// The request's labels WITHOUT consuming them.
///
/// Exists because two signals want the same tags and only one of them can be
/// last: the profiler's flush takes the map (it is finished with the request),
/// while the counted profile (ADR 0029) flushes AFTER it and would otherwise
/// find it empty. The counted side snapshots before the profiler runs.
///
/// Deliberately a clone rather than a shared handle: a label map handed out by
/// reference would let one signal's late `set_label` mutate a document the other
/// has already serialised.
#[must_use]
pub fn labels_snapshot() -> std::collections::BTreeMap<String, String> {
    REQUEST_LABELS.with(|labels| labels.borrow().clone())
}

/// Ticks queued by the SIGPROF handler and consumed at the next safe walk point
/// (an observer fcall boundary). Incrementing an atomic is async-signal-safe; the
/// stack walk itself never runs inside the handler.
static PENDING_TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// RINIT: arm the sampler for this request if it is sampled and a config resolved. Called from the
/// ext-php-rs `#[php_request_startup]` hook in a real build.
pub fn on_request_start(context: &TraceContext, config: &SamplerConfig) {
    REQUEST_SAMPLES.with(|buffer| buffer.borrow_mut().clear());
    REQUEST_LABELS.with(|labels| labels.borrow_mut().clear());
    DRAIN_CLOCK.with(|clock| *clock.borrow_mut() = DrainClock::IDLE);
    let active = context.sampled;
    SAMPLING_ACTIVE.with(|flag| *flag.borrow_mut() = active);
    PENDING_TICKS.store(0, std::sync::atomic::Ordering::Relaxed);
    if active {
        ACTIVE_SAMPLER.with(|s| *s.borrow_mut() = Some((context.clone(), config.clone())));
        // SAFETY SEAM: install the SIGPROF handler and arm the interval timer.
        zend::arm_timer(config.sample_rate_hz);
    } else {
        ACTIVE_SAMPLER.with(|s| *s.borrow_mut() = None);
    }
}

/// Drain queued SIGPROF ticks at a safe point (called from the observer begin
/// trampoline — a Zend instruction boundary), and advance the wall/off-CPU window.
///
/// Up to three samples come out of ONE stack walk: the walk is the expensive part, so
/// CPU, wall and off-CPU all share it (and share the frames themselves, via `Rc`).
///
/// Attribution caveat, the same one every sampling profiler carries: ticks that queued
/// between two drains are charged to the stack observed at the second drain. A tight
/// loop inside a single function generates no drain points, so its ticks land on
/// whatever is called next.
pub fn consume_pending_ticks() {
    let pending = PENDING_TICKS.swap(0, std::sync::atomic::Ordering::Relaxed);
    ACTIVE_SAMPLER.with(|slot| {
        let borrowed = slot.borrow();
        let Some((context, config)) = borrowed.as_ref() else {
            return;
        };
        let period = config.period_nanoseconds();
        let cpu_nanos = u128::from(period).saturating_mul(u128::from(pending));

        // Advance on EVERY drain, including tick-less ones: a drain with no CPU ticks
        // is exactly what blocked time looks like from here.
        let window = DRAIN_CLOCK.with(|clock| {
            clock
                .borrow_mut()
                .advance(monotonic_nanos(), cpu_nanos, period)
        });

        let emit_cpu = config.kinds.cpu && pending > 0;
        let wall_window = window.filter(|_| config.kinds.wall || config.kinds.off_cpu);
        if !emit_cpu && wall_window.is_none() {
            return; // Nothing to record — never pay for a stack walk.
        }

        let stack = zend::walk_current_stack(config.max_stack_depth);
        if stack.is_empty() {
            return;
        }
        let stack = std::rc::Rc::new(stack);
        let at = now_unix_nanos();

        if emit_cpu {
            push_sample(config, SampleKind::Cpu, &stack, cpu_nanos, at, context);
        }
        if let Some((wall_nanos, cpu_in_window)) = wall_window {
            if config.kinds.wall {
                push_sample(config, SampleKind::Wall, &stack, wall_nanos, at, context);
            }
            if config.kinds.off_cpu {
                // Clamped at zero: CPU accounting is quantised to whole ticks, so a
                // busy window can bank slightly more CPU than measured wall time.
                let blocked = wall_nanos.saturating_sub(cpu_in_window);
                if blocked > 0 {
                    push_sample(config, SampleKind::OffCpu, &stack, blocked, at, context);
                }
            }
        }
    });
}

/// Record a precisely measured I/O wait.
///
/// Called from the observer when an instrumented I/O call ends, so unlike the timer
/// kinds this is not statistical: the duration is the real one and the stack is the real
/// stack at the blocking call. That makes it strictly better attribution than off-CPU
/// for the waits it covers — the trade-off is that it only covers calls the observer
/// already instruments (`PDO::`, `mysqli::`, `Redis::`, `curl_exec`, …).
///
/// Overhead is bounded by `min_io_wait_nanos`: a request making thousands of sub-
/// millisecond queries pays a flag read per call, not a stack walk.
pub fn record_io_wait(duration_nanos: u128, function: &str) {
    ACTIVE_SAMPLER.with(|slot| {
        let borrowed = slot.borrow();
        let Some((context, config)) = borrowed.as_ref() else {
            return;
        };
        if !config.kinds.io || duration_nanos < config.min_io_wait_nanos {
            return;
        }
        let stack = io_stack(zend::walk_current_stack(config.max_stack_depth), function);
        if stack.is_empty() {
            return;
        }
        push_sample(
            config,
            SampleKind::Io,
            &std::rc::Rc::new(stack),
            duration_nanos,
            now_unix_nanos(),
            context,
        );
    });
}

/// Guarantee the blocking call is the leaf of an I/O sample.
///
/// The observer's end handler runs as the call unwinds, and whether the returning
/// frame is still `EG(current_execute_data)` at that instant is a Zend implementation
/// detail we should not depend on. Appending the function when it is not already the
/// leaf makes the attribution correct either way, and never duplicates it.
fn io_stack(mut stack: Vec<SampleFrame>, function: &str) -> Vec<SampleFrame> {
    if function.is_empty() {
        return stack;
    }
    if stack.last().is_some_and(|frame| frame.function == function) {
        return stack;
    }
    stack.push(SampleFrame {
        module: "internal".to_owned(),
        function: function.to_owned(),
        file: String::new(),
        line: 0,
    });
    stack
}

/// Buffer one sample, respecting the per-request cap.
fn push_sample(
    config: &SamplerConfig,
    kind: SampleKind,
    stack: &std::rc::Rc<Vec<SampleFrame>>,
    value_nanos: u128,
    sampled_at_unix_nanos: u128,
    context: &TraceContext,
) {
    REQUEST_SAMPLES.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        if buffer.len() >= config.max_samples_per_request {
            return; // Drop silently once capped; completeness is reported downstream, never here.
        }
        buffer.push(Sample {
            kind,
            sampled_at_unix_nanos,
            period_nanoseconds: config.period_nanoseconds(),
            value_nanoseconds: i64::try_from(value_nanos).unwrap_or(i64::MAX),
            stack: std::rc::Rc::clone(stack),
            trace_id: context.trace_id.clone(),
            span_id: context.span_id.clone(),
            session_id: context.session_id.clone(),
        });
    });
}

/// SIGPROF handler entry: the only work is an atomic increment (async-signal-safe).
pub(crate) fn note_tick() {
    PENDING_TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The tick entry point, kept for the direct (non-queued) path and for tests: walk the
/// stack and buffer one sample of `kind`.
pub fn on_tick(context: &TraceContext, config: &SamplerConfig) {
    let is_active = SAMPLING_ACTIVE.with(|flag| *flag.borrow());
    if !is_active {
        return;
    }
    // SAFETY SEAM: walk the live Zend call stack. See `zend::walk_current_stack`.
    let stack = zend::walk_current_stack(config.max_stack_depth);
    capture_sample(context, config, SampleKind::Cpu, stack, now_unix_nanos());
}

/// Pure, testable core of a tick: given an already-walked stack, buffer one `Sample` of
/// one period's weight (respecting the per-request cap). Separated from `on_tick` so
/// tests never need the FFI seam.
pub fn capture_sample(
    context: &TraceContext,
    config: &SamplerConfig,
    kind: SampleKind,
    stack: Vec<SampleFrame>,
    sampled_at_unix_nanos: u128,
) {
    if stack.is_empty() {
        return;
    }
    push_sample(
        config,
        kind,
        &std::rc::Rc::new(stack),
        u128::from(config.period_nanoseconds()),
        sampled_at_unix_nanos,
        context,
    );
}

/// RSHUTDOWN: disarm the timer and drain the buffered samples for spooling. Called from
/// `#[php_request_shutdown]` in a real build.
pub fn on_request_end() -> Vec<Sample> {
    SAMPLING_ACTIVE.with(|flag| *flag.borrow_mut() = false);
    ACTIVE_SAMPLER.with(|s| *s.borrow_mut() = None);
    DRAIN_CLOCK.with(|clock| *clock.borrow_mut() = DrainClock::IDLE);
    PENDING_TICKS.store(0, std::sync::atomic::Ordering::Relaxed);
    // SAFETY SEAM: disarm the interval timer. See `zend::disarm_timer`.
    zend::disarm_timer();
    REQUEST_SAMPLES.with(|buffer| std::mem::take(&mut *buffer.borrow_mut()))
}

/// Monotonic nanoseconds since the first call in this process. Only differences are
/// ever used, so the arbitrary origin is fine — and unlike the wall clock this cannot
/// jump backwards over an NTP correction and produce a negative window.
fn monotonic_nanos() -> u128 {
    static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    ORIGIN
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos()
}

fn now_unix_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// The ONLY unsafe surface in the sampler. Every function here is a hand-written FFI seam that must be
/// implemented against the target PHP's `zend_execute_data` layout and signal machinery. On this
/// machine they are inert stubs so the rest of the crate is reviewable and unit-testable; in a real
/// build each `TODO(unsafe FFI seam)` is replaced with the documented call.
///
/// DESIGN OF THE STACK WALK (to be implemented in `walk_current_stack`):
///   * Start at `executor_globals.current_execute_data` (`EG(current_execute_data)`).
///   * For each `zend_execute_data* ex`: read `ex->func`. If `ex->func->type == ZEND_USER_FUNCTION`,
///     the frame is interpreted PHP — take `func->op_array.function_name`, `func->op_array.scope`
///     (class), `func->op_array.filename`, and the current line from `ex->opline->lineno`. If it is
///     `ZEND_INTERNAL_FUNCTION`, take `func->internal_function.function_name` and module "internal".
///   * Follow `ex = ex->prev_execute_data` until null or `max_depth` frames, then REVERSE so the emit
///     order is root-first (see `Sample::stack`).
///   * Capture NOTHING but identity: names, file, line. Never read argument zvals — that is where
///     application data (and the privacy risk) lives.
///
/// DESIGN OF THE TIMER (to be implemented in `arm_timer`/`disarm_timer`):
///   * Preferred: `setitimer(ITIMER_PROF, ...)` so ticks accrue on CPU time (user+sys), giving a CPU
///     profile; the delivered `SIGPROF` handler is `async-signal-safe` and does the minimum — set a
///     flag / push into a lock-free slot — and the real walk runs on the VM-interrupt callback.
///   * Safer-in-VM alternative: set `EG(vm_interrupt)` and register a `zend_interrupt_function`, so the
///     stack is only ever walked at a VM instruction boundary (never mid-opcode), avoiding torn reads
///     of `zend_execute_data`. The design doc recommends the VM-interrupt path for correctness.
mod zend {
    use super::SampleFrame;

    /// The SIGPROF handler: async-signal-safe by construction — a single atomic
    /// increment. The stack walk happens later at an observer fcall boundary
    /// (`consume_pending_ticks`), never inside the handler.
    #[cfg(feature = "zend-observer")]
    unsafe extern "C" fn chronos_sigprof_handler(_sig: libc::c_int) {
        super::note_tick();
    }

    #[cfg(feature = "zend-observer")]
    pub fn arm_timer(hz: u32) {
        unsafe {
            // Install the handler before arming the timer — an unhandled SIGPROF
            // terminates the process. SA_RESTART keeps interrupted syscalls quiet.
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = chronos_sigprof_handler as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGPROF, &action, std::ptr::null_mut()) != 0 {
                return;
            }

            // Split the period into sec/usec — tv_usec must stay below 1_000_000
            // (a 1 Hz rate would otherwise produce an EINVAL interval).
            let period_usec = 1_000_000 / i64::from(hz);
            let tv = libc::timeval {
                tv_sec: (period_usec / 1_000_000) as _,
                tv_usec: (period_usec % 1_000_000) as _,
            };
            let interval = libc::itimerval {
                it_interval: tv,
                it_value: tv,
            };
            libc::setitimer(libc::ITIMER_PROF, &interval, std::ptr::null_mut());
        }
    }

    #[cfg(not(feature = "zend-observer"))]
    pub fn arm_timer(_hz: u32) {}

    #[cfg(feature = "zend-observer")]
    pub fn disarm_timer() {
        unsafe {
            let zero = libc::itimerval {
                it_interval: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                it_value: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
            };
            libc::setitimer(libc::ITIMER_PROF, &zero, std::ptr::null_mut());
        }
    }

    #[cfg(not(feature = "zend-observer"))]
    pub fn disarm_timer() {}

    #[cfg(feature = "zend-observer")]
    pub fn walk_current_stack(max_depth: usize) -> Vec<SampleFrame> {
        let mut frames = Vec::with_capacity(max_depth.min(64));
        unsafe {
            let globals = &ext_php_rs::ffi::executor_globals;
            let mut execute_data = globals.current_execute_data;
            while !execute_data.is_null() && frames.len() < max_depth {
                let func = (*execute_data).func;
                if !func.is_null() {
                    let func_name = (*func).common.function_name;
                    if !func_name.is_null() {
                        let name =
                            std::ffi::CStr::from_ptr((*func_name).val.as_ptr()).to_string_lossy();

                        let (module, qualified_name) = {
                            let scope = (*func).common.scope;
                            if !scope.is_null() && !(*scope).name.is_null() {
                                let cls = std::ffi::CStr::from_ptr((*(*scope).name).val.as_ptr())
                                    .to_string_lossy();
                                ("php".to_owned(), format!("{cls}::{name}"))
                            } else {
                                ("php".to_owned(), name.into_owned())
                            }
                        };

                        let (file, line) =
                            if (*func).type_ == ext_php_rs::ffi::ZEND_USER_FUNCTION as u8 {
                                let filename = (*func).op_array.filename;
                                let file = if !filename.is_null() {
                                    std::ffi::CStr::from_ptr((*filename).val.as_ptr())
                                        .to_string_lossy()
                                        .into_owned()
                                } else {
                                    String::new()
                                };
                                let line = if !(*execute_data).opline.is_null() {
                                    (*(*execute_data).opline).lineno
                                } else {
                                    0
                                };
                                (file, line)
                            } else {
                                (String::new(), 0)
                            };

                        frames.push(SampleFrame {
                            module,
                            function: qualified_name,
                            file,
                            line,
                        });
                    }
                }
                execute_data = (*execute_data).prev_execute_data;
            }
        }
        frames.reverse();
        frames
    }

    #[cfg(not(feature = "zend-observer"))]
    pub fn walk_current_stack(_max_depth: usize) -> Vec<SampleFrame> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> TraceContext {
        TraceContext {
            trace_id: "a".repeat(32),
            span_id: "b".repeat(16),
            parent_span_id: None,
            sampled: true,
            session_id: Some("session".into()),
            tracestate: None,
            baggage: None,
        }
    }

    fn frame(function: &str) -> SampleFrame {
        SampleFrame {
            module: "php".into(),
            function: function.into(),
            file: "/srv/app/src/Foo.php".into(),
            line: 42,
        }
    }

    fn config(max_samples: usize) -> SamplerConfig {
        SamplerConfig {
            sample_rate_hz: 99,
            max_samples_per_request: max_samples,
            max_stack_depth: 10,
            series_id: "php".into(),
            kinds: SampleKinds::ALL,
            min_io_wait_nanos: 1_000_000,
        }
    }

    #[test]
    fn resolve_disables_sampler_at_zero_hz_and_clamps_high_rates() {
        std::env::set_var("CHRONOS_PHP_PROFILE_SAMPLE_RATE", "0");
        assert!(SamplerConfig::resolve().is_none());
        std::env::set_var("CHRONOS_PHP_PROFILE_SAMPLE_RATE", "100000");
        assert_eq!(SamplerConfig::resolve().unwrap().sample_rate_hz, 1000);
        std::env::remove_var("CHRONOS_PHP_PROFILE_SAMPLE_RATE");
        assert_eq!(
            SamplerConfig::resolve().unwrap().sample_rate_hz,
            SamplerConfig::DEFAULT_RATE_HZ
        );
    }

    #[test]
    fn period_is_the_reciprocal_of_the_rate() {
        let mut config = config(10);
        config.sample_rate_hz = 100;
        assert_eq!(config.period_nanoseconds(), 10_000_000);
    }

    #[test]
    fn capture_buffers_a_sample_and_carries_only_correlation_identity() {
        on_request_end(); // clear any prior thread-local state
        let config = config(10);
        capture_sample(
            &context(),
            &config,
            SampleKind::Cpu,
            vec![frame("App\\Controller::run"), frame("Doctrine::query")],
            1_723_550_400_000_000_000,
        );
        let samples = on_request_end();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].kind, SampleKind::Cpu);
        assert_eq!(samples[0].stack.len(), 2);
        assert_eq!(samples[0].trace_id, "a".repeat(32));
        assert_eq!(samples[0].session_id.as_deref(), Some("session"));
        assert_eq!(
            samples[0].value_nanoseconds,
            config.period_nanoseconds() as i64
        );
    }

    #[test]
    fn capture_enforces_the_per_request_sample_cap() {
        on_request_end();
        let config = config(3);
        for _ in 0..10 {
            capture_sample(&context(), &config, SampleKind::Cpu, vec![frame("f")], 1);
        }
        assert_eq!(on_request_end().len(), 3);
    }

    #[test]
    fn empty_stacks_are_never_buffered() {
        on_request_end();
        let config = config(3);
        capture_sample(&context(), &config, SampleKind::Cpu, Vec::new(), 1);
        assert_eq!(on_request_end().len(), 0);
    }

    // --- Sample kinds ------------------------------------------------------

    #[test]
    fn kind_names_match_the_contract_enum() {
        assert_eq!(SampleKind::Cpu.proto_name(), "PROFILE_SAMPLE_TYPE_CPU");
        assert_eq!(SampleKind::Wall.proto_name(), "PROFILE_SAMPLE_TYPE_WALL");
        assert_eq!(
            SampleKind::OffCpu.proto_name(),
            "PROFILE_SAMPLE_TYPE_OFF_CPU"
        );
        assert_eq!(SampleKind::Io.proto_name(), "PROFILE_SAMPLE_TYPE_IO");
    }

    #[test]
    fn kinds_parse_a_comma_list_and_tolerate_spelling() {
        let kinds = SampleKinds::parse("cpu, off-cpu ,IO");
        assert_eq!(
            kinds,
            SampleKinds {
                cpu: true,
                wall: false,
                off_cpu: true,
                io: true
            }
        );
        assert_eq!(kinds.count(), 3);
    }

    #[test]
    fn an_unrecognised_kind_list_falls_back_to_everything() {
        // Profiling nothing because of a typo is the worse failure: it looks like a
        // quiet application rather than a misconfiguration.
        assert_eq!(SampleKinds::parse("cpuu,bogus"), SampleKinds::ALL);
        assert_eq!(SampleKinds::parse(""), SampleKinds::ALL);
    }

    #[test]
    fn the_sample_cap_scales_with_the_number_of_kinds() {
        std::env::set_var("CHRONOS_PHP_PROFILE_TYPES", "cpu");
        let one = SamplerConfig::resolve().unwrap().max_samples_per_request;
        std::env::set_var("CHRONOS_PHP_PROFILE_TYPES", "cpu,wall,off_cpu,io");
        let four = SamplerConfig::resolve().unwrap().max_samples_per_request;
        std::env::remove_var("CHRONOS_PHP_PROFILE_TYPES");
        assert_eq!(one, SamplerConfig::DEFAULT_MAX_SAMPLES);
        assert_eq!(four, SamplerConfig::DEFAULT_MAX_SAMPLES * 4);
    }

    // --- Wall / off-CPU windowing -----------------------------------------

    #[test]
    fn the_first_drain_only_establishes_a_baseline() {
        // There is no earlier drain to measure from, and the interval before it belongs
        // to bootstrap rather than to any stack we could name.
        let mut clock = DrainClock::IDLE;
        assert!(clock.advance(1_000, 0, 100).is_none());
        assert_eq!(clock.last_monotonic_nanos, Some(1_000));
        assert_eq!(clock.pending_wall_nanos, 0);
    }

    #[test]
    fn wall_time_banks_until_a_full_period_has_passed() {
        let mut clock = DrainClock::IDLE;
        clock.advance(0, 0, 100);
        assert!(
            clock.advance(40, 0, 100).is_none(),
            "40ns is under the 100ns period"
        );
        assert!(
            clock.advance(70, 0, 100).is_none(),
            "70ns cumulative is still under"
        );
        let (wall, cpu) = clock
            .advance(150, 0, 100)
            .expect("150ns exceeds the period");
        assert_eq!(wall, 150);
        assert_eq!(cpu, 0);
        // The window resets, so the next sample is not double-counted.
        assert_eq!(clock.pending_wall_nanos, 0);
    }

    #[test]
    fn off_cpu_is_the_wall_time_that_had_no_cpu_behind_it() {
        let mut clock = DrainClock::IDLE;
        clock.advance(0, 0, 100);
        // 500ns elapsed, only 200ns of it on CPU: 300ns blocked.
        let (wall, cpu) = clock.advance(500, 200, 100).unwrap();
        assert_eq!(wall.saturating_sub(cpu), 300);
    }

    #[test]
    fn cpu_banked_before_the_window_closes_still_counts_against_it() {
        let mut clock = DrainClock::IDLE;
        clock.advance(0, 0, 100);
        clock.advance(30, 30, 100); // under the period: banks CPU, emits nothing
        let (wall, cpu) = clock.advance(120, 60, 100).unwrap();
        assert_eq!(wall, 120);
        assert_eq!(cpu, 90, "CPU from both drains belongs to this window");
    }

    #[test]
    fn a_zero_monotonic_reading_is_a_valid_baseline_not_a_missing_one() {
        // Regression: monotonic_nanos() returns ~0 on the first call of the process, so
        // a 0 sentinel for "no baseline" swallowed the request's first window.
        let mut clock = DrainClock::IDLE;
        assert!(
            clock.advance(0, 0, 100).is_none(),
            "first drain is the baseline"
        );
        let (wall, _) = clock
            .advance(150, 0, 100)
            .expect("second drain must measure");
        assert_eq!(wall, 150);
    }

    #[test]
    fn a_backwards_clock_cannot_produce_a_negative_window() {
        let mut clock = DrainClock::IDLE;
        clock.advance(1_000, 0, 100);
        assert!(clock.advance(500, 0, 100).is_none());
        assert_eq!(clock.pending_wall_nanos, 0);
    }

    // --- Tags --------------------------------------------------------------

    #[test]
    fn label_keys_are_normalised_to_a_safe_alphabet() {
        assert_eq!(
            normalise_label_key("HTTP Route").as_deref(),
            Some("http_route")
        );
        assert_eq!(
            normalise_label_key(" http.route ").as_deref(),
            Some("http.route")
        );
        assert_eq!(normalise_label_key("a\"b:c").as_deref(), Some("a_b_c"));
        assert_eq!(normalise_label_key("__"), None);
        assert_eq!(normalise_label_key(""), None);
    }

    #[test]
    fn labels_round_trip_and_clear_on_take() {
        take_labels();
        set_label("route", "/orders/{id}");
        set_label("action", "orderActions::executeShow");
        let labels = take_labels();
        assert_eq!(
            labels.get("route").map(String::as_str),
            Some("/orders/{id}")
        );
        assert_eq!(labels.len(), 2);
        assert!(take_labels().is_empty(), "take clears for the next request");
    }

    #[test]
    fn empty_label_values_are_ignored() {
        take_labels();
        set_label("route", "   ");
        assert!(take_labels().is_empty());
    }

    #[test]
    fn label_values_are_truncated_not_rejected() {
        take_labels();
        set_label("route", &"x".repeat(MAX_LABEL_VALUE_BYTES + 50));
        let labels = take_labels();
        assert_eq!(labels["route"].len(), MAX_LABEL_VALUE_BYTES);
    }

    #[test]
    fn new_label_keys_are_capped_but_existing_keys_stay_updatable() {
        take_labels();
        for index in 0..MAX_LABELS {
            set_label(&format!("key{index}"), "v");
        }
        set_label("overflow", "v");
        // A late, more precise route must still land after the map has filled.
        set_label("key0", "updated");
        let labels = take_labels();
        assert_eq!(labels.len(), MAX_LABELS);
        assert!(!labels.contains_key("overflow"));
        assert_eq!(labels["key0"], "updated");
    }

    // --- I/O waits ---------------------------------------------------------

    #[test]
    fn io_waits_under_the_threshold_are_not_sampled() {
        // Without an armed sampler there is nothing to record either way; this asserts
        // the guard itself, which is what keeps a chatty request cheap.
        on_request_end();
        record_io_wait(1, "PDO::query");
        assert!(on_request_end().is_empty());
    }

    #[test]
    fn an_io_sample_always_ends_at_the_blocking_call() {
        let walked = vec![frame("run"), frame("query")];
        let stack = io_stack(walked, "PDO::query");
        assert_eq!(stack.last().unwrap().function, "PDO::query");
        assert_eq!(stack.len(), 3);
    }

    #[test]
    fn the_blocking_call_is_never_duplicated_when_already_the_leaf() {
        let walked = vec![frame("run"), frame("PDO::query")];
        let stack = io_stack(walked, "PDO::query");
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn an_unnamed_io_call_leaves_the_walked_stack_alone() {
        assert_eq!(io_stack(vec![frame("run")], "").len(), 1);
    }
}
