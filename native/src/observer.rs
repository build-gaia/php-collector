//! Zend fcall observer with native traceparent injection.
//!
//! MINIT registers `chronos_observer_factory` with `zend_observer_fcall_register`; Zend
//! calls the factory once per observed function and, on each invocation, the returned
//! `begin`/`end` handlers capture spans.
//!
//! SPAN POLICY — the observer sees every call but is deliberately selective about what
//! becomes a span (exhaustive per-call data is the PROFILER's job; framework call trees
//! belong to the continuous profiler, not the trace waterfall):
//!   * Userland-defined functions emit spans ONLY when explicitly allowlisted via the
//!     application's instrumentation manifest (`chronos_trace_function` FFI, fed by
//!     `Chronos\trace_method`). Those spans carry `chronos.instrumented=manifest` and
//!     bypass the min-duration filter. CHRONOS_PHP_SPAN_ALL_USERLAND=1 restores the old
//!     span-every-userland-call behaviour as an escape hatch.
//!     NOTE: the Zend observer caches the factory verdict per function, and a fresh
//!     worker's factory can run before the manifest registers anything — so the factory
//!     attaches handlers to ALL userland functions and the BEGIN handler decides per
//!     call, downgrading unlisted calls to a no-span ObserveOnly frame (which still
//!     pairs with the end handler so the frame stack never desyncs).
//!   * Known I/O internals emit spans WITH payload detail: Redis/Memcached (cache.key),
//!     PDO/mysqli (db.statement), curl_exec (http.url) — span.kind=client.
//!   * Non-deterministic builtins (time/rand/getenv) and curl_setopt are observed for
//!     DST recording / header tracking but never emit spans.
//!   * Leaf spans shorter than CHRONOS_PHP_SPAN_MIN_DURATION_US (default 100µs) are
//!     dropped unless they carry attributes, errored, or kept a child.
//!
//! For outbound curl calls the begin handler injects a W3C `traceparent` header carrying
//! the child span's ID so downstream Chronos-instrumented services link their traces to
//! the caller. Header injection is merge-safe: `curl_setopt($ch, CURLOPT_HTTPHEADER, ...)`
//! calls are observed and the per-handle header list tracked, so injection appends to the
//! application's own headers instead of clobbering them.
//!
//! The begin handler also consumes pending profiler ticks (see `sampler`).

use crate::context::{hex_bytes, TraceContext};
use std::cell::RefCell;

#[derive(Clone, Debug)]
pub struct NativeSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub started_at: String,
    pub ended_at: String,
    pub status: String,
    pub duration_nanoseconds: u128,
    /// Extra span attributes beyond the always-present duration (http.*, error.*, …).
    pub attributes: Vec<(String, String)>,
}

thread_local! {
    static REQUEST_SPANS: RefCell<Vec<NativeSpan>> = const { RefCell::new(Vec::new()) };
    static REQUEST_CONTEXT: RefCell<Option<TraceContext>> = const { RefCell::new(None) };
    static SPAN_STACK: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub struct CallFrame {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    /// The canonical function identity, INTERNED. An `Rc<str>` rather than a `String`
    /// because this used to be the second of two heap allocations every observed call
    /// paid before anything was decided (see `deterministic::NameInterner`); cloning the
    /// interned handle is a refcount bump and the allocation is gone from the hot path.
    name: std::rc::Rc<str>,
    started_at: String,
    /// The SPAN clock. Zero on an `ObserveOnly` frame, deliberately and still: this
    /// field feeds `on_end`'s duration and min-duration logic, and an `ObserveOnly`
    /// frame must never become a span. The deterministic clock is `timing` below —
    /// two fields, two purposes, no possibility of confusing them.
    start_hrtime: u128,
    /// If this call is a network function, the traceparent we injected.
    injected_traceparent: Option<String>,
    /// Whether this frame may become a span at all (ObserveOnly frames never do).
    emit_span: bool,
    /// Whether this frame was pushed onto the parenting stack.
    on_stack: bool,
    /// A kept child forces this span to be kept so the tree stays connected.
    kept_child: bool,
    /// Payload detail captured at call begin (cache.key, db.statement, http.url, …).
    attributes: Vec<(String, String)>,
    /// A display name that replaces the function identity on the emitted span,
    /// when the begin handler derived a better one ("PUBLISH oms.webhooks"
    /// rather than "Bunny\AbstractClient::publish"). `None` everywhere else, and
    /// `on_end` is the only consumer.
    span_name: Option<String>,
    /// This frame opened a message-scoped REQUEST (`begin_messaging_delivery`),
    /// and its end must close it — attaching the scope's LastThrow — before the
    /// frame is retired. A field rather than a name check because a NESTED
    /// dispatch of the same method (a delivery pumped inside an open request)
    /// runs as a plain pass-through frame and must not close a scope it never
    /// opened.
    owns_delivery_scope: bool,
    /// A known I/O call, so its measured duration also becomes an I/O profile sample.
    io: bool,
    /// GAP 2 (causality contract): true only for a `MessagingPublish`/
    /// `MessagingNatsSend`-publish frame whose begin handler ACTUALLY put
    /// this span's id on the wire as a `traceparent` before the frame exists
    /// — a consumer downstream may already be holding that promise by the
    /// time this call returns, so letting `push_span` drop the span at
    /// `MAX_SPANS_PER_REQUEST` would re-create exactly the orphan the
    /// causality contract forbids. Set by the broker-specific begin handler
    /// AFTER it knows whether propagation was attempted and (where a headers
    /// slot exists to fail) whether the write actually succeeded — NOT
    /// derived purely from `policy`, which was the bug the RdKafka contract
    /// caught: `RdKafka\ProducerTopic::produce()` shares `MessagingPublish`
    /// with `producev()` but has no headers argument at all, so nothing is
    /// ever on the wire for it and the promise this flag protects can never
    /// be true. The cap's own docblock calls it a runaway-loop backstop on
    /// MEMORY, not a budget — publish spans are rare and small, so exempting
    /// the ones that really did wire a promise spends nothing a real request
    /// would miss.
    bypass_span_cap: bool,
    /// The DETERMINISTIC clock and this frame's child-time accumulator (ADR 0029).
    /// Carried on the observer's own frame stack so the aggregate never maintains a
    /// parallel stack of its own — which is what would desync it on the requests where
    /// the `ObserveOnly` push is skipped.
    timing: crate::deterministic::FrameTiming,
}

impl CallFrame {
    /// A minimal no-span frame that exists only to pair with the end trampoline's pop
    /// (and to carry the name for DST result recording). Skips span-id generation,
    /// wall-clock formatting, and parent lookup — this runs for every unlisted
    /// userland call, so it must stay cheap.
    fn observe_only(name: std::rc::Rc<str>) -> Self {
        CallFrame {
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: None,
            name,
            started_at: String::new(),
            start_hrtime: 0,
            injected_traceparent: None,
            emit_span: false,
            on_stack: false,
            kept_child: false,
            attributes: Vec::new(),
            span_name: None,
            owns_delivery_scope: false,
            io: false,
            bypass_span_cap: false,
            // Stamped by `push_frame`, which is the only thing allowed to create a
            // counted frame. See its doc comment for why it cannot be stamped here.
            timing: crate::deterministic::FrameTiming::default(),
        }
    }
}

/// What the observer does with an observed call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpanPolicy {
    /// A userland application function: span with no payload capture. Only reachable
    /// via the CHRONOS_PHP_SPAN_ALL_USERLAND escape hatch.
    UserSpan,
    /// A function/method the application's instrumentation manifest allowlisted
    /// (`chronos_trace_function`): span tagged `chronos.instrumented=manifest`,
    /// exempt from the min-duration filter.
    ManifestSpan,
    /// A known I/O call: span + payload attributes + span.kind=client.
    IoSpan,
    /// A vendored messaging client's publish call (`MESSAGING_PUBLISH_METHODS`):
    /// producer span + destination vocabulary + native header injection. Its own
    /// variant rather than a widened `IoSpan` because the begin handler does
    /// something no I/O span does — writes an argument zval — and because the
    /// `/vendor/` excluded-path demotion (which is `UserSpan`-only) must visibly
    /// not apply to it: the whole point is that this IS vendor code.
    MessagingPublish,
    /// A messaging client's own dispatch of one delivery to the application's
    /// callback (`MESSAGING_CONSUME_DISPATCH`): opens and closes a
    /// message-scoped REQUEST rather than emitting a span of its own — the
    /// native replacement for `BunnyTelemetry::consumer`'s wrapper.
    MessagingDeliver,
    /// A PULL-style consume call with no callback boundary at all
    /// (`RdKafka\KafkaConsumer::consume`/`RdKafka\ConsumerTopic::consume`,
    /// `Basis\Nats\Queue::fetchAll`): a plain span, never a message-scoped
    /// request (there is no application code inside this call to bracket —
    /// see the RdKafka contract's "poll-span contract" and the NATS
    /// contract §5(d)). Its own variant rather than `IoSpan` because almost
    /// everything worth saying about the call is only knowable from its
    /// RETURN VALUE — the generic BEGIN (mint a frame if a request is
    /// already open, nothing otherwise) is exactly what this policy wants
    /// with no special-casing, so unlike `IoSpan` it needs no begin-time
    /// `capture_io_detail` branch at all; a dedicated END-time reader
    /// (`capture_messaging_poll_result`) builds the span's name and
    /// attributes from the retval instead.
    MessagingPoll,
    /// NATS's single wire choke point, `Connection::sendMessage` — publish
    /// span+injection when arg 0 is a `Publish` message, subscribe-sid
    /// banking when it is a `Subscribe` message, `ObserveOnly` for anything
    /// else (ping/pong/connect/…). Its own variant because `observe_policy`
    /// caches ONE verdict per function, and this one function serves three
    /// different roles decided only at runtime — see
    /// `MESSAGING_NATS_SEND_METHOD`'s docblock.
    MessagingNatsSend,
    /// Observed for side channels (DST recording, curl header tracking, begin/end
    /// pairing for unlisted userland calls) — no span.
    ObserveOnly,
}

/// Per-PROCESS allowlist of userland functions the instrumentation manifest traced.
/// Grows monotonically over the worker's life (manifests only ever register), so a
/// per-process set is correct: registrations from request 1 stay valid for request N.
fn traced_functions() -> &'static std::sync::RwLock<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::RwLock::new(std::collections::HashSet::new()))
}

/// Called over the `chronos_trace_function` FFI. Names use the observer's qualified
/// format: `Class::method` or a plain `function_name`.
pub fn trace_function(name: &str) {
    let name = name.trim().trim_start_matches('\\');
    if name.is_empty() {
        return;
    }
    if let Ok(mut set) = traced_functions().write() {
        set.insert(name.to_owned());
    }
    // Invalidate every cached policy verdict. `observe_policy` is otherwise pure in the
    // inputs the interner already holds, and the manifest is its ONE mutable input —
    // so bumping a generation here is what makes caching the verdict exactly equivalent
    // to recomputing it, rather than a behaviour change that only shows up on a worker
    // whose manifest registered after its first call to the function.
    MANIFEST_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Bumped by [`trace_function`]. Monotonic per process, and only ever advanced during
/// manifest registration — so in steady state every cached verdict is a hit.
static MANIFEST_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(feature = "zend-observer")]
fn manifest_generation() -> u64 {
    MANIFEST_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// PHP identifiers are case-insensitive, so fall back to a linear case-insensitive
/// scan when the exact-case lookup misses (the set is tiny — a handful of entries).
fn is_traced(name: &str) -> bool {
    traced_functions()
        .read()
        .map(|set| {
            !set.is_empty()
                && (set.contains(name) || set.iter().any(|t| t.eq_ignore_ascii_case(name)))
        })
        .unwrap_or(false)
}

/// Escape hatch restoring the pre-allowlist behaviour of spanning every userland call.
fn span_all_userland() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| crate::settings::flag("CHRONOS_PHP_SPAN_ALL_USERLAND", false))
}

thread_local! {
    /// Native I/O span suppression, set per request by the userland SDK when its
    /// own richer instrumentation is active (Doctrine listener, DB::listen, cache
    /// hooks). The userland span carries db.host/db.name/bound params that the
    /// .so cannot see, so keeping both layers would double-count the same query
    /// and split the service map into a "host not reported" ghost node.
    static SUPPRESS_SQL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SUPPRESS_CACHE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Publish-side messaging suppression: the userland bridge
    /// (`BunnyTelemetry::publish`) already reserved a span id and put it on the
    /// wire, so the native publish observation must neither emit a duplicate
    /// span nor inject anything. PUBLISH ONLY — consume-side ownership cannot be
    /// a per-request flag, because the native delivery scope opens before any
    /// userland code of the delivery runs (see `chronos_suppress_native`'s
    /// docblock in lib.rs); the process switch is `CHRONOS_PHP_MESSAGING_AUTO`.
    static SUPPRESS_MESSAGING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Called over the `chronos_suppress_native` FFI. Unknown kinds are ignored.
pub fn suppress_native(kind: &str) {
    match kind {
        "sql" => SUPPRESS_SQL.with(|flag| flag.set(true)),
        "cache" => SUPPRESS_CACHE.with(|flag| flag.set(true)),
        "messaging" => SUPPRESS_MESSAGING.with(|flag| flag.set(true)),
        _ => {}
    }
}

fn native_io_suppressed(name: &str) -> bool {
    if sql_io_function(name) {
        return SUPPRESS_SQL.with(std::cell::Cell::get);
    }
    if cache_io_method(name) {
        return SUPPRESS_CACHE.with(std::cell::Cell::get);
    }
    false
}

pub fn set_request_context(context: TraceContext) {
    REQUEST_CONTEXT.with(|ctx| *ctx.borrow_mut() = Some(context));
    SPAN_STACK.with(|stack| stack.borrow_mut().clear());
    REQUEST_SPANS.with(|spans| spans.borrow_mut().clear());
    SUPPRESS_SQL.with(|flag| flag.set(false));
    SUPPRESS_CACHE.with(|flag| flag.set(false));
    SUPPRESS_MESSAGING.with(|flag| flag.set(false));
    crate::propagation_priority::reset_for_request();
    #[cfg(feature = "zend-observer")]
    CURL_HEADERS.with(|h| h.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    PREPARED_STATEMENTS.with(|m| m.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    LAST_THROW.with(|t| *t.borrow_mut() = None);
    #[cfg(feature = "zend-observer")]
    MESSAGE_BANK.with(|bank| bank.borrow_mut().clear());
    // CONSUMER_QUEUES is deliberately NOT cleared here: a Bunny subscription
    // outlives every message-scoped request the worker will open (one
    // `Channel::consume`, thousands of deliveries), so per-request clearing
    // would blind the queue lookup after the first message. See its docblock.
}

/// Update the propagation headers on the observer's own copy of the request
/// context. Exists because `enrich_request` (lib.rs) can learn a tracestate /
/// baggage the native start could not see, AFTER `set_request_context` already
/// cloned the context in here — and re-calling `set_request_context` would clear
/// every span recorded during framework bootstrap. Touches ONLY the propagation
/// fields; the trace identity stays whatever the request started with.
pub fn set_propagation(tracestate: Option<String>, baggage: Option<String>) {
    REQUEST_CONTEXT.with(|ctx| {
        if let Some(context) = ctx.borrow_mut().as_mut() {
            context.tracestate = tracestate;
            context.baggage = baggage;
        }
    });
}

pub fn clear_request_context() {
    REQUEST_CONTEXT.with(|ctx| *ctx.borrow_mut() = None);
    SPAN_STACK.with(|stack| stack.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    CURL_HEADERS.with(|h| h.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    PREPARED_STATEMENTS.with(|m| m.borrow_mut().clear());
    #[cfg(feature = "zend-observer")]
    MESSAGE_BANK.with(|bank| bank.borrow_mut().clear());
}

fn on_begin(
    context: &TraceContext,
    function_name: &std::rc::Rc<str>,
    policy: SpanPolicy,
) -> CallFrame {
    let span_id = hex_bytes(8);
    let emit_span = policy != SpanPolicy::ObserveOnly;
    let parent_span_id = SPAN_STACK
        .with(|stack| {
            let stack = stack.borrow();
            stack.last().cloned()
        })
        .or_else(|| Some(context.span_id.clone()));

    if emit_span {
        SPAN_STACK.with(|stack| stack.borrow_mut().push(span_id.clone()));
    }

    // The same pre-send mint the curl path proved: the child span id exists
    // BEFORE the call runs, is put on the wire by the begin handler, and IS
    // recorded when the frame ends — which is the whole causality contract
    // (a traceparent on the wire is a promise the named span is recorded),
    // satisfied with none of the PHP SDK's SpanReservation machinery. Messaging
    // publishes join the network functions here for exactly that reason.
    let traceparent = if is_network_function(function_name) || policy == SpanPolicy::MessagingPublish
    {
        Some(format!(
            "00-{}-{}-{}",
            context.trace_id,
            span_id,
            if context.sampled { "01" } else { "00" }
        ))
    } else {
        None
    };

    CallFrame {
        trace_id: context.trace_id.clone(),
        span_id,
        parent_span_id,
        name: function_name.clone(),
        started_at: now_utc(),
        start_hrtime: monotonic_nanos(),
        injected_traceparent: traceparent,
        emit_span,
        on_stack: emit_span,
        kept_child: false,
        attributes: Vec::new(),
        span_name: None,
        owns_delivery_scope: false,
        io: policy == SpanPolicy::IoSpan,
        // FIX (RdKafka contract, "a real bug this contract surfaces"):
        // `bypass_span_cap` used to be derived purely from `policy ==
        // MessagingPublish`, which was correct for Bunny/`producev()` (their
        // traceparent really is on the wire by the time this frame exists)
        // but WRONG for RdKafka's `produce()` — it has no headers argument at
        // all, so nothing is ever on the wire and there is no promise to
        // protect from the span cap. Each broker's begin handler now sets
        // this explicitly, AFTER it knows whether propagation was actually
        // attempted (and, for the brokers with a headers slot, actually
        // written) — see `begin_messaging_publish_bunny` /
        // `_rdkafka` / `_amqplib` and `begin_nats_send_message`'s Publish
        // branch. `false` here is just the default every OTHER policy keeps.
        bypass_span_cap: false,
        // Stamped by `push_frame`. See its doc comment.
        timing: crate::deterministic::FrameTiming::default(),
    }
}

/// Minimum leaf-span duration in nanoseconds (CHRONOS_PHP_SPAN_MIN_DURATION_US,
/// default 100µs). Spans with payload attributes, errors, or kept children are
/// always kept regardless of duration.
fn min_span_duration_nanos() -> u128 {
    static MIN_NANOS: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    *MIN_NANOS.get_or_init(|| {
        crate::settings::get("CHRONOS_PHP_SPAN_MIN_DURATION_US")
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(100)
            * 1_000
    })
}

/// Finish a frame; returns true when a span was emitted (so the caller can mark
/// the parent frame's `kept_child`).
fn on_end(mut frame: CallFrame, threw: bool) -> bool {
    if frame.on_stack {
        SPAN_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.last().map(|s| s == &frame.span_id).unwrap_or(false) {
                stack.pop();
            }
        });
    }
    if !frame.emit_span {
        return false;
    }

    let duration = monotonic_nanos().saturating_sub(frame.start_hrtime);
    // I/O profile samples are independent of whether the SPAN survives the
    // min-duration filter: a wait long enough to profile is recorded either way, and
    // the sampler applies its own (higher) threshold.
    if frame.io {
        crate::sampler::record_io_wait(duration, &frame.name);
    }
    let keep = threw
        || frame.kept_child
        || !frame.attributes.is_empty()
        || duration >= min_span_duration_nanos();
    if !keep {
        return false;
    }

    // The one place the interned identity is copied into an owned String: a span
    // that actually survives the keep rule. Rare by construction, unlike the
    // per-call allocation this replaced. A begin handler that derived a better
    // display name ("PUBLISH oms.webhooks") wins over the function identity —
    // the destination is the shared identity two services see, where the method
    // name is one side's implementation detail.
    let name = frame
        .span_name
        .take()
        .unwrap_or_else(|| frame.name.to_string());
    let bypass_span_cap = frame.bypass_span_cap;
    let span = NativeSpan {
        trace_id: frame.trace_id,
        span_id: frame.span_id,
        parent_span_id: frame.parent_span_id,
        name,
        started_at: frame.started_at,
        ended_at: now_utc(),
        status: if threw { "error".into() } else { "ok".into() },
        duration_nanoseconds: duration,
        attributes: frame.attributes,
    };
    push_span(span, bypass_span_cap)
}

/// `bypass_cap`: see `CallFrame::bypass_span_cap`'s docblock — GAP 2 exempts a
/// `MessagingPublish` span from `MAX_SPANS_PER_REQUEST` because its
/// traceparent is already on the wire by the time this call is even made.
/// Every other caller passes `false` and keeps the existing backstop.
fn push_span(span: NativeSpan, bypass_cap: bool) -> bool {
    REQUEST_SPANS.with(|spans| {
        let mut spans = spans.borrow_mut();
        if bypass_cap || spans.len() < MAX_SPANS_PER_REQUEST {
            spans.push(span);
            true
        } else {
            false
        }
    })
}

/// Append a userland-recorded span (SpanManager / Doctrine listeners bridged over the
/// `chronos_record_span` FFI) into the same batch the observer spans flush from.
#[allow(clippy::too_many_arguments)]
pub fn record_userland_span(
    trace_id: String,
    span_id: String,
    parent_span_id: String,
    name: String,
    started_at: String,
    ended_at: String,
    status: String,
    attributes: Vec<(String, String)>,
) {
    let parent = if parent_span_id.is_empty() {
        REQUEST_CONTEXT.with(|ctx| ctx.borrow().as_ref().map(|c| c.span_id.clone()))
    } else {
        Some(parent_span_id)
    };
    let trace_id = if trace_id.is_empty() {
        REQUEST_CONTEXT
            .with(|ctx| ctx.borrow().as_ref().map(|c| c.trace_id.clone()))
            .unwrap_or_default()
    } else {
        trace_id
    };
    push_span(
        NativeSpan {
            trace_id,
            span_id,
            parent_span_id: parent,
            name,
            started_at,
            ended_at,
            status,
            duration_nanoseconds: 0,
            attributes,
        },
        // A userland-recorded span is never a MessagingPublish frame (those
        // are native-only), so the cap applies exactly as before GAP 2.
        false,
    );
}

pub fn drain() -> Vec<NativeSpan> {
    REQUEST_SPANS.with(|spans| std::mem::take(&mut *spans.borrow_mut()))
}

/// The request root span. Always emitted at request end so every observer/userland span
/// has an in-batch ancestor and the request itself carries the HTTP identity attributes.
#[allow(clippy::too_many_arguments)]
pub fn root_http_span(
    context: &TraceContext,
    name: &str,
    started_at: String,
    http_status_code: i64,
    request_start_ns: u128,
    request_end_ns: u128,
    status: &str,
    attributes: Vec<(String, String)>,
) -> NativeSpan {
    NativeSpan {
        trace_id: context.trace_id.clone(),
        span_id: context.span_id.clone(),
        parent_span_id: context.parent_span_id.clone(),
        name: if name.is_empty() {
            "request".to_owned()
        } else {
            name.to_owned()
        },
        started_at,
        ended_at: now_utc(),
        status: if !status.is_empty() {
            status.to_owned()
        } else if http_status_code == 0 || (100..500).contains(&http_status_code) {
            "ok".into()
        } else {
            "error".into()
        },
        duration_nanoseconds: request_end_ns.saturating_sub(request_start_ns),
        attributes,
    }
}

/// Functions that make outbound network calls and need traceparent injection.
const NETWORK_FUNCTIONS: &[&str] = &["curl_exec", "curl_multi_exec", "file_get_contents"];

fn is_network_function(name: &str) -> bool {
    NETWORK_FUNCTIONS.contains(&name)
}

/// Messaging PUBLISH calls: producer span + destination vocabulary + native W3C
/// header injection into the client's application-header table, with zero app
/// changes — the replacement for the estates' bespoke publish shims.
///
/// `Bunny\AbstractClient::publish` — the trait method `ClientMethods::publish`
/// (ClientMethods.php:1769), trait-copied into `AbstractClient` (`common.scope`
/// reports `Bunny\AbstractClient`, inherited by pointer into
/// SyncClient/AsyncClient) — chosen over `Bunny\Channel::publish` deliberately
/// and finally:
///
///   * Bunny's own chain `Channel::publish` → `publishImpl` (trait alias) →
///     `AbstractClient::publish` always passes all seven arguments POSITIONALLY,
///     so the headers array is always a real, present argument — the
///     default-argument / `RECV_INIT` clobber problem (see
///     `zend_helpers::inject_array_entries`) never arises at this frame.
///   * It sidesteps trait-alias naming (`publishImpl`'s reported name) entirely,
///     and cannot double-observe: neither `Bunny\Channel::publish` nor
///     `publishImpl` enters any table.
///   * Args here: 0=`$channel`(int), 1=`$body`, 2=`$headers`(array),
///     3=`$exchange`, 4=`$routingKey` — and `$this` IS the client, so its
///     protected `$options` (vhost, host, port) is one native property read
///     away. That read is what makes this path strictly better than the bespoke
///     bridge, which had to take the vhost as an argument and could never set
///     `server.address` at all (the "broker not identified" map node).
///
/// This is vendor code and stays observable anyway: the `/vendor/` excluded-path
/// demotion applies only to `SpanPolicy::UserSpan` (see the begin trampoline),
/// never to a table-named policy.
///
/// UPDATE (RdKafka/amqplib waves): the FOLLOW-UPS this docblock used to list
/// have landed — see `begin_messaging_publish_rdkafka` and
/// `begin_messaging_publish_amqplib` for their own begin handlers, and their
/// contracts (`chronos-desktop`'s task record) for the reasoning behind each
/// broker's shape. The corrected premise for amqplib: injection does NOT need
/// a userland call after all — `$msg->properties['application_headers']` has
/// no runtime type enforcement, so a raw PHP array is exactly as legal a wire
/// value as a real `AMQPTable` (see `inject_amqp_application_headers`'s
/// docblock). NATS is a separate table below (`Connection::sendMessage`
/// serves publish AND subscribe banking by runtime type, so it cannot share a
/// name-keyed policy with these three).
///
/// STILL listed, not built — each its own follow-up, cited by its contract:
///   * RdKafka: the `Conf::set`/`newTopic` handle-keyed side-channel bank that
///     would recover `server.address`/cluster/consumer.group (v2 candidate;
///     needs a new librdkafka C dependency to do properly — see the RdKafka
///     contract's "vhost/cluster/server.address recovery" section).
///   * amqplib: `batch_basic_publish`/`publish_batch` (messages queued and
///     wire-written later, in a loop this table entry never sees) and
///     `basic_get` (a one-shot PULL API with no callback boundary — a poll
///     span is possible, a message-scoped request is not, same reasoning as
///     RdKafka's own pull API — but out of scope of the callback-consumer ask
///     this wave targeted).
#[cfg_attr(not(feature = "zend-observer"), allow(dead_code))]
const MESSAGING_PUBLISH_METHODS: &[&str] = &[
    "Bunny\\AbstractClient::publish",
    "RdKafka\\ProducerTopic::produce",
    "RdKafka\\ProducerTopic::producev",
    "PhpAmqpLib\\Channel\\AMQPChannel::basic_publish",
];

/// Messaging consume DISPATCH points: the client's own method that hands one
/// delivery to the application callback — the only stable native scope boundary
/// a raw AMQP consumer has (there is no framework seam, no envelope, no stamp).
///
/// `Bunny\Channel::onBodyComplete` (Channel.php:707) is where a completed
/// deliver frame becomes `$callback($message, $this, $this->client)` (:743).
/// Its begin opens a message-scoped request ('QUEUE', the queue name, the
/// traceparent from the message's application headers); its end closes it,
/// attaching the scope's LastThrow — which is what replaces the estates'
/// explicit `consumeFailed()` call sites with no app code at all.
///
/// `PhpAmqpLib\Channel\AMQPChannel::basic_deliver` joins it for the identical
/// reason (amqplib contract, "Consume scoping"): its whole body IS
/// `call_user_func($this->callbacks[$consumer_tag], $message)` plus
/// delivery-info bookkeeping, so it is the only stable native callback
/// boundary this client has. `Basis\Nats\Client::processMsg` joins for the
/// same shape (NATS contract §5(a)) — its own begin handler additionally
/// declines (falls to `ObserveOnly`) when the dispatch target is a `Queue`
/// buffer (§5(b), no application code runs) or an RPC reply to this client's
/// own `dispatch()` (§5(c)), neither of which is a real inbound job.
/// `close_messaging_delivery` is broker-agnostic (it only reads `LastThrow`
/// and `DELIVERY_SCOPE`) and is reused verbatim for all three.
const MESSAGING_CONSUME_DISPATCH: &[&str] = &[
    "Bunny\\Channel::onBodyComplete",
    "PhpAmqpLib\\Channel\\AMQPChannel::basic_deliver",
    "Basis\\Nats\\Client::processMsg",
];

/// Messaging consume SUBSCRIBE points, observed as a side channel only (like
/// `curl_setopt`): `Bunny\Channel::consume`'s END banks
/// `(channel handle, retval->consumerTag) → arg-1 queue` into `CONSUMER_QUEUES`
/// so the dispatch scope can name the queue this consumer actually asked for.
/// `Channel::run` needs no entry — it calls `consume()` internally, which is
/// observed. The AsyncClient promise retval banks nothing and naming falls back
/// to routing key / exchange, honestly.
///
/// `PhpAmqpLib\Channel\AMQPChannel::basic_consume` joins for the identical
/// reason — its retval is the FINAL consumer tag (server-assigned when the
/// caller passed `''`), a plain STRING rather than Bunny's
/// `MethodBasicConsumeOkFrame` object, so its own END-time banking function
/// (`maybe_bank_amqplib_consumer_queue`) reads the retval differently even
/// though it writes into the SAME `CONSUMER_QUEUES` map (see
/// `ConsumeSubscribeKind`, which tells the two apart at END without a second
/// map). NATS needs no entry here at all: subscribe banking for NATS happens
/// inside `Connection::sendMessage`'s OWN begin handler
/// (`begin_nats_send_message`'s `Subscribe` branch) — its `sid` is minted
/// client-side, before the wire write, so there is no "wait for the server's
/// OK frame" step to bank at END the way Bunny/amqplib need.
const MESSAGING_CONSUME_SUBSCRIBE: &[&str] = &[
    "Bunny\\Channel::consume",
    "PhpAmqpLib\\Channel\\AMQPChannel::basic_consume",
];

/// PULL-style consume calls with no callback boundary at all — see
/// `SpanPolicy::MessagingPoll`'s docblock. `RdKafka\ConsumerTopic::consume` is
/// the legacy per-topic form; `RdKafka\KafkaConsumer::consume` is the modern
/// one. `Basis\Nats\Queue::fetchAll` is JetStream's pull-batch fetch (NATS
/// contract §5(d)) — `Consumer::handle()`, which calls it internally and then
/// invokes the app's handler in its own `foreach`, gets no span of its own
/// (same reasoning as `Bunny\Channel::run`, which also just calls an observed
/// method).
const MESSAGING_POLL_METHODS: &[&str] = &[
    "RdKafka\\KafkaConsumer::consume",
    "RdKafka\\ConsumerTopic::consume",
    "Basis\\Nats\\Queue::fetchAll",
];

/// NATS's single wire choke point (NATS contract §0): every publish AND every
/// subscribe registration passes through this ONE method, disambiguated only
/// at runtime by the class of arg 0 (`Basis\Nats\Message\Publish` vs
/// `Basis\Nats\Message\Subscribe`) — `observe_policy` cannot express that with
/// a fixed per-function verdict the way the name-keyed tables above do, so it
/// gets its own `SpanPolicy` (`MessagingNatsSend`) whose begin handler
/// (`begin_nats_send_message`) does the runtime dispatch instead.
const MESSAGING_NATS_SEND_METHOD: &str = "Basis\\Nats\\Connection::sendMessage";

/// NATS's synchronous request/reply wrapper (NATS contract §6): `span.kind =
/// client`, RPC-shaped, not a messaging producer+consumer pair — the
/// underlying `publish()` call already gets its own full PUBLISH span via
/// `Connection::sendMessage` (nested inside this one), so this span only adds
/// the "this was a round trip, not fire-and-forget" fact. Folded into the
/// generic `IoSpan` policy (`is_messaging_rpc` joins the `IoSpan` eligibility
/// check) rather than given its own `SpanPolicy` variant: unlike
/// `MessagingPoll`, everything this span needs (the subject argument, the
/// destination vocabulary) is knowable at BEGIN from a plain scalar argument,
/// exactly the shape `capture_io_detail`'s other branches already handle.
const MESSAGING_NATS_RPC_METHOD: &str = "Basis\\Nats\\Client::dispatch";

/// Process kill switch for the whole native messaging table set:
/// `CHRONOS_PHP_MESSAGING_AUTO`, default ON; an explicit off empties every
/// messaging table (publish, dispatch, subscribe) at once. Resolved once per
/// process — the same `OnceLock` shape as `span_all_userland`, and that
/// process-stability is what makes it safe under `cached_policy`.
///
/// This flag is also the CONSUME-side ownership switch: unlike publish (which
/// the per-request `suppress_native("messaging")` seam covers), a userland
/// consumer wrapper runs strictly AFTER the native dispatch scope has opened,
/// so no per-request declaration can reach the decision in time.
fn messaging_auto() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| crate::settings::flag("CHRONOS_PHP_MESSAGING_AUTO", true))
}

fn is_messaging_publish(name: &str) -> bool {
    messaging_auto() && MESSAGING_PUBLISH_METHODS.contains(&name)
}

fn is_messaging_dispatch(name: &str) -> bool {
    messaging_auto() && MESSAGING_CONSUME_DISPATCH.contains(&name)
}

fn is_messaging_subscribe(name: &str) -> bool {
    messaging_auto() && MESSAGING_CONSUME_SUBSCRIBE.contains(&name)
}

fn is_messaging_poll(name: &str) -> bool {
    messaging_auto() && MESSAGING_POLL_METHODS.contains(&name)
}

fn is_nats_send(name: &str) -> bool {
    messaging_auto() && name == MESSAGING_NATS_SEND_METHOD
}

fn is_messaging_rpc(name: &str) -> bool {
    messaging_auto() && name == MESSAGING_NATS_RPC_METHOD
}

/// GAP 1's other observed point: the protobuf runtime's own serializer. This
/// estate's publishers never set the AMQP `type` header (`begin_messaging_publish`'s
/// PRIMARY source of `messaging.message.name`), so nothing marks a publish
/// body with its schema class — and without that name the schema registry
/// cannot map a span to a protobuf type at all.
///
/// The fix needs no application code because `serializeToString`'s RETURN
/// VALUE is the exact bytes about to be published and `$this` is exactly the
/// generated DTO class the registry needs — see `bank_serialized_message`'s
/// docblock for how the two are joined to a later publish body.
///
/// One name only, not a class-prefix table like `CACHE_CLASS_PREFIXES`:
/// every generated message, whatever its own namespace, calls this ONE
/// inherited method (`Google\Protobuf\Internal\Message::serializeToString`),
/// never overriding it — so matching the declaring method's qualified name
/// covers every schema class in one entry, with no per-class registration.
const PROTOBUF_SERIALIZE_METHOD: &str = "Google\\Protobuf\\Internal\\Message::serializeToString";

fn messaging_auto_serialize_hook(name: &str) -> bool {
    messaging_auto() && name == PROTOBUF_SERIALIZE_METHOD
}

/// Stream functions that are network calls only SOMETIMES. `file_get_contents` is the
/// one that matters: the same builtin fetches an HTTP URL and reads a local template,
/// and the overwhelmingly common case is the local read. Spanning both put dozens of
/// 0 ms `file_get_contents` rows into every trace, which is noise that buries the
/// request's real work — so the target decides, per call, not the function name.
const DUAL_PURPOSE_STREAM_FUNCTIONS: &[&str] = &["file_get_contents"];

fn is_dual_purpose_stream_function(name: &str) -> bool {
    DUAL_PURPOSE_STREAM_FUNCTIONS.contains(&name)
}

/// Stream wrappers that cross the network. Deliberately an allow-list: `php://`,
/// `data://`, `compress.*://`, a bare relative path and everything else unnamed here
/// are local, and a wrapper nobody listed should read as local rather than quietly
/// producing a client span for a file read.
const REMOTE_STREAM_SCHEMES: &[&str] = &[
    "http://", "https://", "ftp://", "ftps://", "sftp://", "ssh2.",
];

/// Whether a stream target actually goes over the network. Scheme comparison is
/// case-insensitive because PHP's wrapper lookup is.
fn is_remote_stream_target(target: &str) -> bool {
    let target = target.trim_start();
    REMOTE_STREAM_SCHEMES.iter().any(|scheme| {
        target.len() >= scheme.len() && target[..scheme.len()].eq_ignore_ascii_case(scheme)
    })
}

/// SQL calls that ARE the I/O. Method-scoped, not class-scoped: PDO/PDOStatement
/// expose dozens of cursor/metadata methods (setAttribute, fetch, bindParam,
/// closeCursor, …) that are per-row bookkeeping, not I/O — spanning them buried
/// real queries under hundreds of 0ms rows.
const SQL_IO_FUNCTIONS: &[&str] = &[
    "PDO::query",
    "PDO::exec",
    "PDO::prepare",
    "PDOStatement::execute",
    "mysqli::query",
    "mysqli::real_query",
    "mysqli::execute_query",
    "mysqli_query",
    // Prepared-statement execution IS the round trip — `mysqli::prepare` /
    // `SQLite3::prepare` only compile the statement. The SQL text is not an
    // argument at execute time, so it is captured at prepare time into
    // PREPARED_STATEMENTS (keyed by the statement object's handle, same
    // per-handle-map pattern as CURL_HEADERS) and stamped onto this span.
    "mysqli_stmt::execute",
    "SQLite3::query",
    "SQLite3::exec",
    "SQLite3::querySingle",
    "SQLite3Stmt::execute",
];

/// The prepare calls whose RESULT is a statement object worth remembering.
/// `PDO::prepare` is deliberately absent: it already emits its own `IoSpan`
/// carrying `db.statement`, and userland PDO instrumentation (Doctrine,
/// DB::listen) owns the richer capture there. Procedural `mysqli_prepare`
/// belongs here too (same returned-statement shape, SQL at arg 1 after the
/// link) — see `prepare_sql_arg_index`.
const SQL_PREPARE_METHODS: &[&str] = &["mysqli::prepare", "SQLite3::prepare", "mysqli_prepare"];

/// The prepare calls that mutate the statement object they are CALLED ON
/// (`$db->stmt_init()` + `$stmt->prepare($sql)`, `new mysqli_stmt($link, $sql)`)
/// rather than returning a fresh one. Observed for the same side channel as
/// SQL_PREPARE_METHODS, with one extra duty the retval-keyed path never has:
/// EVICTION. The Zend engine recycles object handles within a request, so a
/// statement created through one of these calls can inherit the handle id of a
/// freed statement whose SQL is still banked — and a later `::execute` would
/// then be stamped with the OLD query's text. Every observed call here therefore
/// either re-banks the handle with its real SQL or evicts it (failed prepare,
/// unreadable argument): a wrong query on a span is worse than no query.
const SQL_THIS_PREPARE_METHODS: &[&str] = &["mysqli_stmt::prepare", "mysqli_stmt::__construct"];

fn sql_prepare_method(name: &str) -> bool {
    SQL_PREPARE_METHODS.contains(&name)
}

fn sql_this_prepare_method(name: &str) -> bool {
    SQL_THIS_PREPARE_METHODS.contains(&name)
}

/// Which argument carries the SQL text for a given prepare call. The method
/// forms take it first; the procedural forms and the constructor take the
/// connection/link first and the SQL second.
fn prepare_sql_arg_index(name: &str) -> usize {
    match name {
        "mysqli_prepare" | "mysqli_stmt::__construct" => 1,
        _ => 0,
    }
}

/// Cache client classes whose DATA methods are I/O spans. Lifecycle methods
/// (construct/connect/auth/…) are excluded from observation entirely.
const CACHE_CLASS_PREFIXES: &[&str] = &["Redis::", "RedisCluster::", "Memcached::"];

const LIFECYCLE_METHODS: &[&str] = &[
    "__construct",
    "__destruct",
    "connect",
    "pconnect",
    "open",
    "auth",
    "select",
    "close",
    "setOption",
    "getOption",
    "addServer",
    "addServers",
    "quit",
];

fn sql_io_function(name: &str) -> bool {
    SQL_IO_FUNCTIONS.contains(&name)
}

fn cache_io_method(name: &str) -> bool {
    CACHE_CLASS_PREFIXES.iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|method| !LIFECYCLE_METHODS.contains(&method))
    })
}

/// Non-deterministic builtins whose results a DST recording captures so a replay can
/// substitute them. Names are lowercase (internal functions report lowercase).
fn dst_event_kind_for(name: &str) -> Option<crate::dst_spool::DstEventKind> {
    use crate::dst_spool::DstEventKind as K;
    Some(match name {
        "time" | "microtime" | "hrtime" | "date" | "mktime" | "gmdate" => K::Time,
        "rand" | "mt_rand" | "random_int" | "random_bytes" | "uniqid" | "mt_srand" => K::Random,
        "getenv" => K::EnvRead,
        _ => return None,
    })
}

/// Per-request span collection ceiling. This bounds MEMORY, not the wire: the
/// flush splits spans into byte-bounded spool files (see lib.rs), so a deep
/// request never hits ingest's 1 MiB per-batch body limit. Effectively
/// unbounded for real requests — a runaway-loop backstop, not a budget:
/// 32k spans × ~1KB worst-case buffered ≈ 32 MB transient per request.
const MAX_SPANS_PER_REQUEST: usize = 32_768;

/// Userland namespaces that are infrastructure, not application logic. Their
/// calls never become spans (the profiler still sees them).
const SKIP_PREFIXES: &[&str] = &[
    "Composer\\Autoload\\",
    "Chronos\\Collector\\",
    "sfAutoload::",
    "sfCoreAutoload::",
    "sfSimpleAutoload::",
    "Illuminate\\Container\\",
    "Illuminate\\Support\\",
    "Illuminate\\Events\\",
    "Illuminate\\Pipeline\\Pipeline::carry",
    "Illuminate\\Foundation\\Application::isDeferredService",
    "Illuminate\\Foundation\\Application::bound",
];

/// Path fragments whose code is not this application's. A userland span defined in a
/// file matching any of them is dropped: dependency internals are the profiler's
/// territory, where they are attributed without flooding the trace waterfall.
///
/// Defaults cover the PHP and JS dependency trees; override per service with
/// `CHRONOS_PHP_EXCLUDE_PATHS` (comma-separated fragments, matched as substrings of the
/// defining file path). An explicitly EMPTY value means "exclude nothing" — a service
/// that really wants to trace inside its dependencies can say so.
const DEFAULT_EXCLUDED_PATHS: &[&str] = &["/vendor/", "/node_modules/", "/cache/", "/.git/"];

fn excluded_paths() -> &'static Vec<String> {
    static PATHS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    PATHS.get_or_init(|| match crate::settings::get("CHRONOS_PHP_EXCLUDE_PATHS") {
        Some(list) => parse_excluded_paths(&list),
        None => DEFAULT_EXCLUDED_PATHS
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    })
}

/// Parse the env list. Blank entries are dropped rather than becoming a fragment that
/// matches every path — one stray comma should not silence a service's whole trace.
fn parse_excluded_paths(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a defining file belongs to code the service does not own. An UNKNOWN file
/// (internal function, eval'd code, closure with no op array) is never excluded — the
/// rule drops what it can positively identify as a dependency, and a missing path is
/// not evidence.
fn is_excluded_path(file: &str, excluded: &[String]) -> bool {
    if file.is_empty() {
        return false;
    }
    excluded
        .iter()
        .any(|fragment| file.contains(fragment.as_str()))
}

/// Decide what to do with an observed call. `is_internal` is true for engine
/// builtins (strpos, curl_*, Redis::get, …), false for userland-defined code.
///
/// CACHING CAVEAT: the Zend engine caches the observer factory's verdict per function,
/// and on a fresh worker the factory can run before the instrumentation manifest has
/// registered anything. This function therefore never returns `None` for a plain
/// userland function — the factory attaches handlers, and the BEGIN handler re-runs
/// this per call so a later `chronos_trace_function` registration takes effect. An
/// unlisted userland call gets `ObserveOnly`: a paired begin/end frame, no span.
fn observe_policy(name: &str, is_internal: bool) -> Option<SpanPolicy> {
    if name.is_empty() || name.starts_with("chronos_") {
        return None;
    }
    if name == "curl_setopt" || name == "curl_setopt_array" {
        return Some(SpanPolicy::ObserveOnly);
    }
    // Observed for the side channel only, like curl_setopt above: the prepare
    // call is where the SQL text is last visible, so its end handler banks the
    // text against the statement object (returned, or `$this` for the mutating
    // forms) for the later `::execute` span. No span of its own — the wire
    // round trip that matters is the execute.
    if sql_prepare_method(name) || sql_this_prepare_method(name) {
        return Some(SpanPolicy::ObserveOnly);
    }
    // Messaging tables, checked BEFORE the `is_internal` early-return below
    // because these are userland vendor methods — and immune to the `/vendor/`
    // excluded-path demotion, which applies to `UserSpan` only (see the begin
    // trampoline): being vendor code is the point of a name table.
    if is_messaging_publish(name) {
        return Some(SpanPolicy::MessagingPublish);
    }
    if is_messaging_dispatch(name) {
        return Some(SpanPolicy::MessagingDeliver);
    }
    if is_messaging_subscribe(name) {
        // Side channel only, like curl_setopt: the END handler banks the
        // consumerTag → queue mapping. No span.
        return Some(SpanPolicy::ObserveOnly);
    }
    if is_messaging_poll(name) {
        return Some(SpanPolicy::MessagingPoll);
    }
    if is_nats_send(name) {
        // The runtime-typed publish/subscribe/neither dispatch — see
        // `MESSAGING_NATS_SEND_METHOD`'s docblock. `RdKafka::produce`/
        // `producev` are both C-extension methods (`is_internal` true for
        // both) and this NATS method is userland — all still checked here,
        // ahead of the `is_internal` return below, for the identical reason
        // the other messaging tables are.
        return Some(SpanPolicy::MessagingNatsSend);
    }
    if messaging_auto_serialize_hook(name) {
        // Checked here rather than after the `is_internal` return below for
        // exactly the reason the messaging tables are: whichever protobuf
        // runtime an estate ships (the C extension's compiled `Message`, or
        // the pure-PHP composer fallback) this method must be observed either
        // way — the C extension form IS internal, and would be silently
        // skipped by the `is_internal` early return two branches down.
        // Side channel only, like `curl_setopt` / the subscribe table above:
        // the END handler banks `$this`'s class against the returned bytes
        // (`bank_serialized_message`). No span of its own.
        return Some(SpanPolicy::ObserveOnly);
    }
    if dst_event_kind_for(name).is_some() {
        return Some(SpanPolicy::ObserveOnly);
    }
    if is_network_function(name)
        || sql_io_function(name)
        || cache_io_method(name)
        || is_messaging_rpc(name)
    {
        return Some(SpanPolicy::IoSpan);
    }
    if is_internal {
        // Engine builtins (strpos, substr, file_exists, …) are profiler
        // territory — never spans.
        return None;
    }
    // Closures/{main} can never be named by a manifest — safe to cache a hard no.
    if name == "{main}" || name.starts_with("{closure") || name.contains("\\{closure") {
        return None;
    }
    if is_traced(name) {
        return Some(SpanPolicy::ManifestSpan);
    }
    if !span_all_userland() {
        // Framework call trees are the profiler's job — no span, but keep the
        // frame paired so a per-process allowlist registration can kick in later.
        return Some(SpanPolicy::ObserveOnly);
    }
    if SKIP_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
        return None;
    }
    // Class autoloaders are infrastructure whatever the framework calls them —
    // their cost is compile time, which the profiler attributes correctly.
    if name.ends_with("::autoload") || name.ends_with("::loadClass") {
        return None;
    }
    Some(SpanPolicy::UserSpan)
}

/// Whether MINIT actually registered the observer. False when the collector was
/// explicitly switched off at module startup, and false in a build without the
/// `zend-observer` feature.
static OBSERVER_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether this process has a live Zend observer. The heartbeat reports it, because
/// a process that later resolves `enabled` to true while this is false will produce
/// no observer spans and nothing else would say why.
pub fn installed() -> bool {
    OBSERVER_INSTALLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Register the fcall observer, unless the collector is explicitly off.
///
/// Registration is MINIT-only — `zend_observer_fcall_register` cannot be called
/// later — and it is not free: an installed observer costs roughly 30ns on EVERY
/// userland call, whether or not a request is being collected, because the factory
/// and the paired begin/end handlers run regardless. That is the price of the
/// feature when the collector is on, and pure waste when it is off, which is the
/// case for an image that bakes the .so in and disables it.
///
/// Only an EXPLICIT off skips it (`CHRONOS_PHP_ENABLED` / `chronos.enabled` in the
/// process environment or php.ini). An absent setting installs as before, because a
/// value this early cannot see per-request configuration — see `settings::startup_flag`.
/// The consequence is worth stating plainly: switching the collector off at module
/// startup and back on per-request leaves the process without an observer for its
/// whole life. `heartbeat()` says so out loud when that happens.
pub fn install_observer() {
    if !crate::settings::startup_flag("CHRONOS_PHP_ENABLED", true) {
        return;
    }
    #[cfg(feature = "zend-observer")]
    unsafe {
        zend_observer_fcall_register(Some(chronos_observer_factory));
        // Chain rather than clobber: another extension (or a future chronos component)
        // may already have set this single global slot, and there is no registration
        // list to append to — only one function pointer.
        let previous = ext_php_rs::ffi::zend_throw_exception_hook;
        let _ = PREVIOUS_THROW_HOOK.set(previous);
        ext_php_rs::ffi::zend_throw_exception_hook = Some(chronos_throw_trampoline);
        OBSERVER_INSTALLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(not(feature = "zend-observer"))]
    {}
}

#[cfg(feature = "zend-observer")]
static PREVIOUS_THROW_HOOK: std::sync::OnceLock<
    Option<unsafe extern "C" fn(ex: *mut ext_php_rs::ffi::zend_object)>,
> = std::sync::OnceLock::new();

/// The most recent throw of the request, bounded like the root span's own error caps
/// (`lib.rs` caps error.type at 256 and error.message at 2048 — same numbers here, so
/// what shutdown stamps can never exceed what a bridge could have stamped).
#[derive(Clone, Debug)]
pub struct LastThrow {
    pub class: String,
    pub message: String,
    pub file: String,
    pub line: String,
}

#[cfg(feature = "zend-observer")]
thread_local! {
    /// Set by the throw hook on EVERY throw while a request is open, overwritten by
    /// later throws, cleared at request start. At RSHUTDOWN this is the only witness
    /// left of an exception that escaped a frameworkless script — the object itself
    /// is gone by then. See `lib.rs::uncaught_exception_at_shutdown` for how it is
    /// matched against `error_get_last()` before anything is stamped.
    static LAST_THROW: RefCell<Option<LastThrow>> = const { RefCell::new(None) };
}

/// The request's most recent recorded throw, if any. `None` without the observer
/// feature — there is no throw hook to have seen one.
pub fn last_throw() -> Option<LastThrow> {
    #[cfg(feature = "zend-observer")]
    {
        LAST_THROW.with(|t| t.borrow().clone())
    }
    #[cfg(not(feature = "zend-observer"))]
    {
        None
    }
}

/// Truncate on a char boundary — same shape as `lib.rs::cap`, local so the throw
/// hook does not reach across modules for a three-line helper.
#[cfg(feature = "zend-observer")]
fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Native throw-time exception capture (debug-extension direction P5, gated on this native
/// `.so` existing — see the phasing doc). `zend_throw_exception_hook` fires once per throw,
/// before any catch block runs, with the raw `Throwable` object — the only point at which
/// class/message/file/line are cheaply available without walking a stack. This is DST-only
/// (a replay recording), distinct from and upstream of the userland `ExceptionCaptureRecorder`
/// (P1) that builds the `chronos.debugging.v1` snapshot for the Errors/Snapshot feature.
///
/// Recorded regardless of whether the exception is ultimately caught: a caught internal
/// exception still shaped the execution path a replay must reproduce.
#[cfg(feature = "zend-observer")]
unsafe extern "C" fn chronos_throw_trampoline(exception: *mut ext_php_rs::ffi::zend_object) {
    // The identity is read at most once per throw, and only when someone will use
    // it: the DST recording, or the request-open LAST_THROW slot that lets a
    // frameworkless fatal still stamp error.* onto its root span at RSHUTDOWN.
    let request_open = REQUEST_CONTEXT.with(|ctx| ctx.borrow().is_some());
    if request_open || crate::dst_spool::is_active() {
        if let Some(identity) = zend_helpers::exception_identity(exception) {
            if request_open {
                LAST_THROW.with(|slot| {
                    *slot.borrow_mut() = Some(LastThrow {
                        class: bounded(&identity.class, 256),
                        message: bounded(identity.message.as_deref().unwrap_or(""), 2048),
                        file: bounded(identity.file.as_deref().unwrap_or(""), 1024),
                        line: bounded(identity.line.as_deref().unwrap_or(""), 16),
                    });
                });
            }
            if crate::dst_spool::is_active() {
                let mut payload = vec![("class".to_owned(), identity.class)];
                if let Some(message) = identity.message {
                    payload.push(("message".to_owned(), message));
                }
                if let Some(code) = identity.code {
                    payload.push(("code".to_owned(), code));
                }
                if let Some(file) = identity.file {
                    payload.push(("file".to_owned(), file));
                }
                if let Some(line) = identity.line {
                    payload.push(("line".to_owned(), line));
                }
                crate::dst_spool::record(crate::dst_spool::DstEventKind::Exception, payload);
            }
        }
    }
    if let Some(Some(previous)) = PREVIOUS_THROW_HOOK.get() {
        previous(exception);
    }
}

#[cfg(feature = "zend-observer")]
type ZendObserverFcallBegin =
    unsafe extern "C" fn(execute_data: *mut ext_php_rs::ffi::zend_execute_data);

#[cfg(feature = "zend-observer")]
type ZendObserverFcallEnd = unsafe extern "C" fn(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
);

#[cfg(feature = "zend-observer")]
#[repr(C)]
struct ZendObserverFcallHandlers {
    begin: Option<ZendObserverFcallBegin>,
    end: Option<ZendObserverFcallEnd>,
}

#[cfg(feature = "zend-observer")]
type ZendObserverFcallInit = unsafe extern "C" fn(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) -> ZendObserverFcallHandlers;

#[cfg(feature = "zend-observer")]
unsafe extern "C" {
    fn zend_observer_fcall_register(callback: Option<ZendObserverFcallInit>);
}

#[cfg(feature = "zend-observer")]
unsafe extern "C" fn chronos_observer_factory(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) -> ZendObserverFcallHandlers {
    const NONE: ZendObserverFcallHandlers = ZendObserverFcallHandlers {
        begin: None,
        end: None,
    };
    if execute_data.is_null() {
        return NONE;
    }
    let name = zend_helpers::function_name(execute_data).unwrap_or_default();
    let is_internal = zend_helpers::is_internal_function(execute_data);
    if observe_policy(&name, is_internal).is_none() {
        return NONE;
    }
    ZendObserverFcallHandlers {
        begin: Some(chronos_begin_trampoline),
        end: Some(chronos_end_trampoline),
    }
}

/// ADR 0021 Phase 2: retain a first-party call visit when DST is armed.
///
/// ADR 0029 Tier 3 deepens the same event rather than adding a parallel one:
/// `arguments` (already allowlisted, typed, redacted and capped by
/// `capture_tier_three_arguments`) ride the existing `call` payload as
/// `arg<N>.name` / `arg<N>.type` / `arg<N>.value` keys. A path-diff that can say WHICH
/// argument the two runs disagreed on is a materially deeper execution graph than one
/// that can only say the same function was reached, and it costs no new event kind, no
/// new spool schema and no second budget.
#[cfg(feature = "zend-observer")]
fn record_call_path_enter(
    name: &str,
    is_internal: bool,
    defining_file: Option<&str>,
    arguments: &[crate::deterministic::CapturedArgument],
) {
    if is_internal || !crate::dst_spool::is_active() {
        return;
    }
    let first_party = crate::call_path::is_first_party(defining_file, excluded_paths());
    let caps = crate::call_path::caps();
    if let Some(depth) = crate::call_path::on_enter(&caps, first_party) {
        let mut payload = vec![
            ("name".to_owned(), name.to_owned()),
            ("depth".to_owned(), depth.to_string()),
        ];
        for argument in arguments {
            let prefix = format!("arg{}", argument.position);
            if !argument.name.is_empty() {
                payload.push((format!("{prefix}.name"), argument.name.clone()));
            }
            // The TYPE is always recorded, including for a composite whose value was
            // refused: "argument 3 was an array" is evidence about the path and leaks
            // nothing.
            payload.push((
                format!("{prefix}.type"),
                argument.argument_type.proto_name().to_owned(),
            ));
            if !argument.value.is_empty() {
                payload.push((format!("{prefix}.value"), argument.value.clone()));
            }
            if argument.redacted {
                payload.push((format!("{prefix}.redacted"), "true".to_owned()));
            }
            if argument.truncated {
                payload.push((format!("{prefix}.truncated"), "true".to_owned()));
            }
        }
        crate::dst_spool::record(crate::dst_spool::DstEventKind::Call, payload);
    }
}

/// One call's function identity, resolved through the per-process interner.
///
/// This is where the two heap allocations every observed call used to pay before
/// anything was decided are removed: `zend_helpers::function_name`'s
/// `format!("{class}::{method}")` and `CallFrame::observe_only`'s owned copy now happen
/// once per FUNCTION per PROCESS. A hit costs one map probe and three refcount bumps.
///
/// Returns `None` only when the runtime has no function to describe at all, which is
/// also the case in which the old code produced an empty name and `observe_policy`
/// refused it.
#[cfg(feature = "zend-observer")]
unsafe fn interned_function(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) -> Option<crate::deterministic::InternedFunction> {
    let handle = zend_helpers::function_ptr(execute_data);
    if handle == 0 {
        // No stable key to intern against. Fall back to interning by NAME — a string
        // comparison per call rather than per function, which is the price of not
        // having a key, but never a wrong identity and never a missing row.
        return Some(crate::deterministic::intern_named(
            zend_helpers::function_facts(execute_data)?,
        ));
    }
    Some(crate::deterministic::intern(handle, || {
        zend_helpers::function_facts(execute_data).unwrap_or_default()
    }))
}

/// The observer's span verdict for an interned function, cached per function.
///
/// WHY THIS IS SAFE TO CACHE, given the warning on `observe_policy` itself. That
/// warning is about the ZEND factory's cache, which decides whether handlers are
/// attached AT ALL and cannot be revisited. This cache is ours, it is keyed by
/// [`crate::deterministic::FunctionId`], and it is invalidated by
/// [`MANIFEST_GENERATION`] — the only mutable input `observe_policy` has. Every other
/// input (the name, the internal flag, `span_all_userland`'s `OnceLock`) is fixed for
/// the life of the process, so a generation-matched hit is byte-identical to a
/// recomputation.
///
/// A `Vec` indexed by id rather than a second map: ids are dense and monotonic, so this
/// is an array index, and it keeps the deterministic module free of any knowledge of
/// `SpanPolicy`.
#[cfg(feature = "zend-observer")]
fn cached_policy(
    id: crate::deterministic::FunctionId,
    name: &str,
    is_internal: bool,
) -> Option<SpanPolicy> {
    let generation = manifest_generation();
    let index = id as usize;
    let hit = POLICY_CACHE.with(|cache| {
        cache
            .borrow()
            .get(index)
            .copied()
            .flatten()
            .filter(|entry| entry.generation == generation)
            .map(|entry| entry.policy)
    });
    if let Some(policy) = hit {
        return policy;
    }
    let policy = observe_policy(name, is_internal);
    POLICY_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() <= index {
            cache.resize(index + 1, None);
        }
        cache[index] = Some(CachedPolicy { generation, policy });
    });
    policy
}

#[cfg(feature = "zend-observer")]
#[derive(Clone, Copy)]
struct CachedPolicy {
    generation: u64,
    policy: Option<SpanPolicy>,
}

/// Tier 3: bounded, redacted, scalar-only arguments for a manifest-allowlisted call.
///
/// FOUR independent gates, all of which must pass, and none of which this function is
/// allowed to skip: the request must have armed Tier 3 (a forced profile or an armed
/// DST recording), the function must be userland, and it must be named by the
/// instrumentation manifest. `CHRONOS_PHP_PROFILE_ARGS` is the fourth and is checked
/// inside `arguments_active`.
///
/// Returns the captured arguments so the caller can ALSO hang them on the DST
/// call-path event — the same evidence, once, in both places a reader looks.
#[cfg(feature = "zend-observer")]
unsafe fn capture_tier_three_arguments(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    interned: &crate::deterministic::InternedFunction,
) -> Vec<crate::deterministic::CapturedArgument> {
    if interned.internal || !crate::deterministic::arguments_active() {
        return Vec::new();
    }
    // The manifest allowlist, re-checked here rather than trusted from the policy:
    // `ManifestSpan` is not the only policy an allowlisted function can end up with
    // (an allowlisted I/O method keeps `IoSpan`), and Tier 3 follows the ALLOWLIST, not
    // the span decision.
    if !is_traced(&interned.name) {
        return Vec::new();
    }
    let config = crate::deterministic::config();
    let (arguments, dropped) = zend_helpers::capture_scalar_arguments(
        execute_data,
        config.max_arguments,
        config.max_argument_bytes,
    );
    if arguments.is_empty() && dropped == 0 {
        return Vec::new();
    }
    if crate::deterministic::record_arguments(interned.id, arguments.clone(), dropped) {
        arguments
    } else {
        // Refused by a budget. The refusal is already counted in `truncation`; handing
        // the arguments to the DST graph anyway would route around the byte cap.
        Vec::new()
    }
}

/// Push a prepared frame onto the observer's stack, stamping the deterministic clock in
/// the same breath.
///
/// THE ONE PLACE a counted frame comes into existence, and that is the point:
/// `deterministic::on_enter` increments a call count and a recursion depth that only the
/// matching pop can unwind, so an enter that is not immediately followed by a push is a
/// frame the end trampoline can never balance.
///
/// It also has to be LAST rather than merely paired. `inject_curl_traceparent` calls back
/// into PHP — `curl_setopt`, which is itself an observed function with its own
/// trampolines — so a start stamped before the frame was assembled would charge our own
/// header injection to the observed call, and would hand the injected call's duration to
/// this frame's parent while this frame's inclusive time covered it too. Double counted,
/// plausible-looking, and invisible.
#[cfg(feature = "zend-observer")]
fn push_frame(mut frame: CallFrame, function: crate::deterministic::FunctionId) {
    frame.timing = crate::deterministic::on_enter(function, monotonic_nanos_u64());
    CALL_FRAMES.with(|frames| frames.borrow_mut().push(frame));
}

#[cfg(feature = "zend-observer")]
fn record_call_path_leave(is_internal: bool) {
    if is_internal || !crate::dst_spool::is_active() {
        return;
    }
    crate::call_path::on_leave();
}

#[cfg(feature = "zend-observer")]
unsafe extern "C" fn chronos_begin_trampoline(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) {
    if execute_data.is_null() {
        return;
    }

    // Consume any profiler ticks the SIGPROF handler queued since the last
    // function-call boundary. This is the safe walk point: we are at a Zend
    // instruction boundary, never inside a signal handler. MUST stay first: ticks are
    // drained at the instruction boundary, not deferred behind our own bookkeeping.
    crate::sampler::consume_pending_ticks();

    // ONE dereference of `(*execute_data).func`, cached per function: name, defining
    // file, line and the internal flag all come off the same struct, and the old shape
    // re-walked the pointer once per helper once per call.
    let Some(interned) = interned_function(execute_data) else {
        return;
    };
    let name = interned.name.clone();
    let is_internal = interned.internal;
    let Some(mut policy) = cached_policy(interned.id, &name, is_internal) else {
        return;
    };

    // Userland SQL/cache instrumentation is active for this request: its spans
    // are richer (host/db/bound params), so the native ones stand down. The
    // frame still runs as ObserveOnly to keep begin/end pairing intact.
    if policy == SpanPolicy::IoSpan && native_io_suppressed(&name) {
        policy = SpanPolicy::ObserveOnly;
    }

    // Native messaging observation, handled ahead of the generic paths because
    // every one of these does something no other policy does: a publish writes
    // an argument (or property) zval and must do part of it even with no open
    // request (the enqueued-at stamp), a dispatch opens/closes a whole request,
    // and NATS's single send method decides its own role from arg 0's runtime
    // type. Each handler keeps the begin/end frame pairing itself, mirroring
    // the ObserveOnly branch's guard exactly on every pass-through path.
    if policy == SpanPolicy::MessagingPublish {
        if SUPPRESS_MESSAGING.with(std::cell::Cell::get) {
            // The userland bridge (`BunnyTelemetry::publish`) declared ownership
            // for this request: its reserved span id is already on the wire, so
            // the native path emits no duplicate span and writes no header —
            // demoted to a plain paired frame, exactly like a suppressed I/O
            // span. (Even unsuppressed, the caller-wins hash-add could not have
            // clobbered the bridge's traceparent; suppression removes the SPAN.)
            policy = SpanPolicy::ObserveOnly;
        } else {
            // One name-keyed table, three broker shapes — dispatched here
            // rather than inside one shared function, because the argument
            // layout, the header-injection mechanic and the destination
            // vocabulary genuinely differ per broker (see each function's
            // own docblock); only the SpanPolicy and the surrounding
            // begin/end pairing are shared.
            match &*name {
                "RdKafka\\ProducerTopic::produce" | "RdKafka\\ProducerTopic::producev" => {
                    begin_messaging_publish_rdkafka(execute_data, &name, &interned)
                }
                "PhpAmqpLib\\Channel\\AMQPChannel::basic_publish" => {
                    begin_messaging_publish_amqplib(execute_data, &name, &interned)
                }
                _ => begin_messaging_publish_bunny(execute_data, &name, &interned),
            }
            return;
        }
    } else if policy == SpanPolicy::MessagingDeliver {
        match &*name {
            "PhpAmqpLib\\Channel\\AMQPChannel::basic_deliver" => {
                begin_messaging_delivery_amqplib(execute_data, &name, &interned)
            }
            "Basis\\Nats\\Client::processMsg" => {
                begin_messaging_delivery_nats(execute_data, &name, &interned)
            }
            _ => begin_messaging_delivery_bunny(execute_data, &name, &interned),
        }
        return;
    } else if policy == SpanPolicy::MessagingNatsSend {
        begin_nats_send_message(execute_data, &name, &interned);
        return;
    } else if policy == SpanPolicy::MessagingPoll && SUPPRESS_MESSAGING.with(std::cell::Cell::get) {
        // Symmetry with the `MessagingPublish` demotion above (RdKafka
        // contract, "Suppression seam"): a userland bridge that declared
        // messaging ownership for this request covers the poll side too.
        policy = SpanPolicy::ObserveOnly;
    }

    // A dual-purpose stream call is a client span only when its target is actually
    // remote. Read once here: the verdict decides the policy, and a remote target is
    // also the span's `http.url`, so re-reading the argument later would be waste.
    let mut stream_url = None;
    if policy == SpanPolicy::IoSpan && is_dual_purpose_stream_function(&name) {
        let target = zend_helpers::arg_scalar_string(execute_data, 0, 2048);
        match target {
            Some(target) if is_remote_stream_target(&target) => stream_url = Some(target),
            // A local file read. The profiler still accounts for its time; the trace
            // does not need a row per template.
            _ => policy = SpanPolicy::ObserveOnly,
        }
    }

    // Where the observed userland function is DEFINED — the file that owns the code,
    // not the file that called it. Used both to drop dependency internals and, when a
    // span survives, to tell the reader which source file it came from.
    //
    // Read off the INTERNED origin rather than re-resolved per call: a function's
    // defining file is fixed for the life of the process, so this was a third heap
    // allocation on every observed call for a string that never changes.
    let defining_file: Option<&str> = if is_internal { None } else { interned.file() };
    // Manifest spans are explicit intent (`chronos_trace_function`) and are NEVER
    // dropped by the path rule: someone asked for that function by name, and a
    // dependency they chose to instrument is theirs to see.
    if policy == SpanPolicy::UserSpan
        && defining_file.is_some_and(|file| is_excluded_path(file, excluded_paths()))
    {
        policy = SpanPolicy::ObserveOnly;
    }

    // Track curl header configuration so traceparent injection is merge-safe.
    if &*name == "curl_setopt" {
        track_curl_setopt(execute_data);
    } else if &*name == "curl_setopt_array" {
        track_curl_setopt_array(execute_data);
    } else if is_messaging_subscribe(&name) {
        // Remember the subscribe method's own zend_function pointer, TAGGED
        // with which broker's END-time banking shape it needs, so the END
        // trampoline can recognise it with one pointer scan (the end handler
        // has no interned frame for calls made outside any request, which is
        // exactly when a subscribe call runs — worker startup). The banking
        // itself happens at end, where the retval exists. See
        // `maybe_bank_consumer_queue` and `ConsumeSubscribeKind`.
        let kind = if &*name == "PhpAmqpLib\\Channel\\AMQPChannel::basic_consume" {
            ConsumeSubscribeKind::Amqplib
        } else {
            ConsumeSubscribeKind::Bunny
        };
        let function = zend_helpers::function_ptr(execute_data);
        CONSUME_FNS.with(|fns| {
            let mut fns = fns.borrow_mut();
            if !fns.iter().any(|(known, _)| *known == function) {
                fns.push((function, kind));
            }
        });
    }

    // ObserveOnly is the bulk path (every unlisted userland call lands here): push a
    // minimal frame purely to keep begin/end pairing intact — no span ids, no
    // timestamps, no context clone. The end trampoline pops it and emits nothing.
    if policy == SpanPolicy::ObserveOnly {
        let has_context = REQUEST_CONTEXT.with(|ctx| ctx.borrow().is_some());
        // Pair begin/end whenever we have a request context OR DST is armed: call-path
        // depth must stay balanced with leave in the end trampoline.
        //
        // The deterministic aggregate rides STRICTLY inside this guard, and that is
        // load-bearing. The end trampoline pops unconditionally, so counting an enter
        // the guard refused to push would leave a frame the pop can never balance —
        // and the aggregate would drift a little further wrong on every request. The
        // buffer therefore never owns a stack; it is handed back the timing this frame
        // carries. `has_context` is true for every request the collector actually
        // started, sampled or not, which is exactly Tier 1's always-on scope.
        if has_context || crate::dst_spool::is_active() {
            let arguments = capture_tier_three_arguments(execute_data, &interned);
            record_call_path_enter(&name, is_internal, defining_file, &arguments);
            push_frame(CallFrame::observe_only(name), interned.id);
        }
        return;
    }

    REQUEST_CONTEXT.with(|ctx| {
        let context = ctx.borrow().as_ref().cloned();
        if let Some(context) = context {
            let mut frame = on_begin(&context, &name, policy);

            if policy == SpanPolicy::IoSpan {
                capture_io_detail(execute_data, &mut frame);
                if let Some(ref url) = stream_url {
                    frame
                        .attributes
                        .push((crate::http_capture::URL_FULL.into(), url.clone()));
                    frame.attributes.push(("http.url".into(), url.clone()));
                }
            }
            // The source file behind a userland span. Internal functions have none,
            // and this is the same identity the profiler already reports per frame —
            // never an argument or a captured value.
            if let Some(file) = defining_file {
                frame
                    .attributes
                    .push(("code.filepath".into(), file.to_owned()));
            }
            if policy == SpanPolicy::ManifestSpan {
                // Explicit user intent: the attribute both marks provenance for the UI
                // and exempts the span from the min-duration filter (the keep rule
                // always retains spans that carry attributes).
                frame
                    .attributes
                    .push(("chronos.instrumented".into(), "manifest".into()));
            }

            if let Some(ref traceparent) = frame.injected_traceparent {
                if &*name == "curl_exec" {
                    inject_curl_traceparent(execute_data, traceparent);
                }
                // curl_multi_exec / file_get_contents: covered by the pending-
                // traceparent userland seam (Guzzle middleware, Http facade).
            }

            let arguments = capture_tier_three_arguments(execute_data, &interned);
            record_call_path_enter(&name, is_internal, defining_file, &arguments);
            // Same enter-and-push as the ObserveOnly branch above, through the same
            // helper, so Tier 1 covers ALL observed calls rather than only the unlisted
            // ones — and so neither branch can drift from the other.
            push_frame(frame, interned.id);
        }
    });
}

/// Attach payload detail to an I/O span from the observed call's arguments.
/// Values are bounded; bound parameters stay in the userland hooks (Doctrine /
/// DB::listen) where redaction policy applies.
#[cfg(feature = "zend-observer")]
unsafe fn capture_io_detail(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    frame: &mut CallFrame,
) {
    let name: &str = &frame.name;
    frame.attributes.push(("span.kind".into(), "client".into()));
    // Arg 0 is a cache KEY only on data operations — on lifecycle methods
    // (__construct, connect, auth, …) it is a host or credential, not a key.
    let lifecycle_method = name.rsplit("::").next().is_some_and(|method| {
        matches!(
            method,
            "__construct"
                | "__destruct"
                | "connect"
                | "pconnect"
                | "open"
                | "auth"
                | "select"
                | "close"
                | "setOption"
                | "getOption"
                | "addServer"
                | "addServers"
                | "quit"
        )
    });
    if name.starts_with("Redis::") || name.starts_with("RedisCluster::") {
        frame.attributes.push(("db.system".into(), "redis".into()));
        if !lifecycle_method {
            if let Some(key) = zend_helpers::arg_scalar_string(execute_data, 0, 256) {
                frame.attributes.push(("cache.key".into(), key));
            }
        }
    } else if name.starts_with("Memcached::") {
        frame
            .attributes
            .push(("db.system".into(), "memcached".into()));
        if !lifecycle_method {
            if let Some(key) = zend_helpers::arg_scalar_string(execute_data, 0, 256) {
                frame.attributes.push(("cache.key".into(), key));
            }
        }
    } else if name == "PDO::query"
        || name == "PDO::exec"
        || name == "PDO::prepare"
        || name == "mysqli::query"
        || name == "SQLite3::query"
        || name == "SQLite3::exec"
    {
        if let Some(statement) = zend_helpers::arg_scalar_string(execute_data, 0, 4096) {
            // Legacy + current semconv spelling, same value — see the constants'
            // comment in `http_capture.rs` for why both are kept.
            frame
                .attributes
                .push((crate::http_capture::DB_QUERY_TEXT.into(), statement.clone()));
            frame.attributes.push(("db.statement".into(), statement));
        }
    } else if name == "mysqli_stmt::execute" || name == "SQLite3Stmt::execute" {
        // The SQL text is NOT an argument here — it was banked at prepare time
        // against this statement object's handle. A map miss (procedural prepare,
        // cap overflow, statement from before this request) still emits the span,
        // just without the text: an execute round trip with an unknown statement
        // is better evidence than no row at all.
        if let Some(handle) = zend_helpers::this_object_handle(execute_data) {
            let statement = PREPARED_STATEMENTS.with(|map| map.borrow().get(&handle).cloned());
            if let Some(statement) = statement {
                frame
                    .attributes
                    .push((crate::http_capture::DB_QUERY_TEXT.into(), statement.clone()));
                frame.attributes.push(("db.statement".into(), statement));
            }
        }
    } else if name == "curl_exec" {
        if let Some(url) = zend_helpers::curl_effective_url(execute_data) {
            frame
                .attributes
                .push((crate::http_capture::URL_FULL.into(), url.clone()));
            frame.attributes.push(("http.url".into(), url));
        }
    } else if name == MESSAGING_NATS_RPC_METHOD {
        // NATS contract §6: `span.kind=client` is already pushed above (every
        // `IoSpan` gets it); everything else this span needs is `dispatch`'s
        // own arg 0 — the "name" it sends and blocks for a reply against, the
        // same fact the RPC-shaped span is named after. The underlying
        // publish (via `Connection::sendMessage`, nested inside this call)
        // gets its own full PUBLISH span with the payload facts — this span
        // only adds "this was a round trip," not a second copy of the
        // message vocabulary.
        use crate::messaging;
        if let Some(subject) = zend_helpers::arg_scalar_string(execute_data, 0, 512) {
            frame
                .attributes
                .push(("messaging.system".into(), messaging::SYSTEM_NATS.into()));
            let destination = messaging::nats_destination(&subject);
            for (key, value) in destination.attributes() {
                frame.attributes.push((key.into(), value));
            }
            frame.span_name = Some(messaging::labeled(
                "NATS.request",
                &destination,
                messaging::SYSTEM_NATS,
            ));
        }
    }
}

#[cfg(feature = "zend-observer")]
unsafe extern "C" fn chronos_end_trampoline(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
) {
    // A function call observed while an exception is propagating (or that itself
    // threw) unwinds with EG(exception) set — mark the span errored.
    let threw = zend_helpers::exception_pending();

    // Consumer-subscription banking runs OUTSIDE the frame stack, because
    // `Channel::consume` usually runs where no frame was pushed at all (worker
    // startup, no request open, CLI auto-start off — the ObserveOnly push is
    // context-gated). The recognition inside is one pointer compare per end
    // call, so every other function pays essentially nothing for it.
    maybe_bank_consumer_queue(execute_data, retval);

    CALL_FRAMES.with(|frames| {
        let frame = frames.borrow_mut().pop();
        if let Some(mut frame) = frame {
            // The DETERMINISTIC clock is read FIRST, before any of the bookkeeping
            // below: curl result capture calls back into PHP (`curl_getinfo`) and DST
            // recording formats strings, and charging our own instrumentation's cost to
            // the observed function would be the one bias a counted profiler must not
            // have.
            let leave_nanos = monotonic_nanos_u64();

            // Bank the deterministic aggregate immediately, BEFORE the curl/DST
            // bookkeeping below and before `on_end` consumes the frame.
            //
            // Ordering, not taste: `capture_curl_result` calls back into PHP
            // (`curl_getinfo`), and doing our own accounting first means no
            // re-entrant observed call can interleave with it. The caller is read off
            // the frame BELOW this one on the observer's OWN stack — the same stack the
            // pop came from — so a Tier 2 edge can never name a caller the frame stack
            // does not actually have.
            let timing = frame.timing;
            let caller = frames.borrow().last().map(|parent| parent.timing.function);
            let duration = crate::deterministic::on_leave(&timing, caller, leave_nanos);
            // Exclusive time is `duration - child_nanoseconds`, so every frame hands its
            // inclusive duration up to its parent as it leaves. Done even when the
            // aggregate refused to count THIS frame (the function cap), because the
            // parent's exclusive time is still wrong without it.
            if let Some(parent) = frames.borrow_mut().last_mut() {
                parent.timing.child_nanoseconds =
                    parent.timing.child_nanoseconds.saturating_add(duration);
            }
            // Everything an outbound HTTP call knows is only knowable now: curl fills
            // its timing and response info in during the transfer, and the body is the
            // return value we are holding.
            if &*frame.name == "curl_exec" {
                capture_curl_result(execute_data, retval, &mut frame);
            }
            // Bank a successful prepare's SQL against the statement object — the one
            // it RETURNED (mysqli::prepare and friends) or the one it was CALLED ON
            // (mysqli_stmt::prepare / new mysqli_stmt) — for the later `::execute`
            // span (the text is not an argument there). At END rather than begin
            // because only the end handler holds the return value — and a failed
            // prepare (`false`) must bank nothing / must EVICT a recycled handle.
            if sql_prepare_method(&frame.name) {
                bank_prepared_statement(execute_data, retval, prepare_sql_arg_index(&frame.name));
            } else if sql_this_prepare_method(&frame.name) {
                rebank_prepared_statement_for_this(execute_data, retval, &frame.name);
            } else if &*frame.name == PROTOBUF_SERIALIZE_METHOD {
                // GAP 1: only the END handler holds the serialized bytes (the
                // return value) — see `bank_serialized_message`.
                bank_serialized_message(execute_data, retval);
            } else if MESSAGING_POLL_METHODS.contains(&frame.name.as_ref()) {
                // `SpanPolicy::MessagingPoll`'s END-time reader: almost
                // everything worth saying about a pull-style consume call is
                // only knowable from its RETURN VALUE — see that policy's
                // own docblock.
                capture_messaging_poll_result(execute_data, retval, &mut frame);
            }
            // DST: record the observed result of known non-deterministic builtins.
            if crate::dst_spool::is_active() {
                if let Some(kind) = dst_event_kind_for(&frame.name) {
                    let payload = if matches!(kind, crate::dst_spool::DstEventKind::EnvRead) {
                        // Protocol env channel selects on variable name and answers with value
                        // (conformance / Effect::environment), not function+result.
                        let name_arg = zend_helpers::arg_scalar_string(execute_data, 0, 4096)
                            .unwrap_or_default();
                        let value = zend_helpers::scalar_to_string(retval).unwrap_or_default();
                        vec![("name".to_owned(), name_arg), ("value".to_owned(), value)]
                    } else {
                        let value = zend_helpers::scalar_to_string(retval).unwrap_or_default();
                        vec![
                            ("function".to_owned(), frame.name.to_string()),
                            ("result".to_owned(), value),
                        ]
                    };
                    crate::dst_spool::record(kind, payload);
                }
                let is_internal = zend_helpers::is_internal_function(execute_data);
                record_call_path_leave(is_internal);
            }
            // A frame that OPENED a message-scoped request closes it before it
            // is retired — after the DST bookkeeping above (the recording must
            // still be active when the leave is counted) and before `on_end`
            // (which for this ObserveOnly-shaped frame emits nothing anyway).
            if frame.owns_delivery_scope {
                close_messaging_delivery(execute_data, threw);
            }
            if on_end(frame, threw) {
                // A kept child pins its ancestor so the waterfall stays connected.
                if let Some(parent) = frames.borrow_mut().last_mut() {
                    parent.kept_child = true;
                }
            }
        }
    });
}

#[cfg(feature = "zend-observer")]
thread_local! {
    static CALL_FRAMES: RefCell<Vec<CallFrame>> = const { RefCell::new(Vec::new()) };
    /// Cached `observe_policy` verdicts, indexed by interned function id. Per PROCESS
    /// in spirit (PHP-FPM is one request per worker), and deliberately NOT reset
    /// between requests: the verdict depends only on the function and the manifest
    /// generation, both of which outlive the request. See `cached_policy`.
    static POLICY_CACHE: RefCell<Vec<Option<CachedPolicy>>> = const { RefCell::new(Vec::new()) };
    /// Per-curl-handle custom header lists observed via curl_setopt, keyed by the
    /// CurlHandle object handle id. Injection merges with these instead of clobbering.
    static CURL_HEADERS: RefCell<std::collections::HashMap<u32, Vec<String>>> =
        RefCell::new(std::collections::HashMap::new());
    /// SQL text banked at prepare time (`mysqli::prepare` / `SQLite3::prepare` /
    /// `mysqli_prepare` keyed by the RETURNED statement's handle;
    /// `mysqli_stmt::prepare` / `new mysqli_stmt` keyed by `$this` — the query is
    /// not an argument of the later `::execute` call, so this map is the only
    /// bridge between the two). Same per-handle-map pattern as CURL_HEADERS,
    /// cleared with it per request, and EVICTED per handle whenever an observed
    /// prepare path re-creates a statement without readable SQL — the engine
    /// recycles object handles, and a recycled id keeping a freed statement's text
    /// would stamp the wrong query onto a span. A miss (evicted entry, statement
    /// prepared before this request) degrades to an execute span WITHOUT the text
    /// — never to a dropped span.
    static PREPARED_STATEMENTS: RefCell<std::collections::HashMap<u32, String>> =
        RefCell::new(std::collections::HashMap::new());
    /// `(channel object handle, consumer tag) → queue name`, banked at
    /// `Bunny\Channel::consume`'s end so the delivery scope can name the queue
    /// this consumer actually asked for (the deliver frame itself only carries
    /// exchange + routing key). Unlike CURL_HEADERS / PREPARED_STATEMENTS this
    /// is NOT cleared in `set_request_context`: a subscription outlives every
    /// message-scoped request the worker opens — one `consume()`, thousands of
    /// deliveries — so per-request clearing would blind the lookup after the
    /// first message. Process lifetime, capped, insert-overwrites: consumer tags
    /// are server-unique per channel, so an object-handle recycle colliding with
    /// a still-live identical tag is a non-issue in practice — and the overwrite
    /// means the newest subscription always wins anyway.
    static CONSUMER_QUEUES: RefCell<std::collections::HashMap<(u32, String), String>> =
        RefCell::new(std::collections::HashMap::new());
    /// The `zend_function` pointers of every observed subscribe method seen
    /// this PROCESS (`Bunny\Channel::consume`, `AMQPChannel::basic_consume`),
    /// each tagged with which broker's END-time retval shape it needs —
    /// banked at BEGIN so the END trampoline can recognise it with a small
    /// linear scan (the end handler often has no frame for it: subscription
    /// happens outside any request, where `ObserveOnly` frames are not
    /// pushed). A `Vec`, not the single `Cell` an earlier version of this
    /// design used: that assumed exactly one subscribe method existed at
    /// all, which stopped being true the moment a second broker's table
    /// entry landed — TWO different vendored clients can be loaded in one
    /// estate, and the old single-slot design would have let the second
    /// one's registration silently clobber the first's, breaking whichever
    /// broker's subscribe call ran less recently. Bounded by construction: as
    /// many entries as there are subscribe methods in the messaging tables
    /// (currently 2), so the scan is never a real cost. Function structs are
    /// process-lifetime engine allocations, the same stability the interner
    /// relies on.
    static CONSUME_FNS: RefCell<Vec<(usize, ConsumeSubscribeKind)>> =
        RefCell::new(Vec::new());
    /// The route of the message-scoped request the CURRENT delivery frame
    /// opened, taken by the close. Depth-1 by construction: a nested dispatch
    /// hits the already-active guard and never owns a scope. See
    /// `DeliveryRoute`'s docblock for why amqplib defers the actual name to
    /// close time instead of knowing it here.
    static DELIVERY_SCOPE: RefCell<Option<DeliveryRoute>> = const { RefCell::new(None) };
    /// NATS subscribe-side banking (NATS contract §4): `sid → (subject,
    /// group)`, banked inside `begin_nats_send_message`'s `Subscribe` branch
    /// — client-minted BEFORE the wire write, so (unlike Bunny/amqplib's
    /// server-assigned consumer tags) there is no "wait for the server's OK
    /// frame" step; banking happens at the same begin handler already doing
    /// publish-side work, no END hook needed. Process lifetime, like
    /// CONSUMER_QUEUES: a subscription outlives every message-scoped request
    /// the worker opens.
    static SUBSCRIBE_SIDS: RefCell<std::collections::HashMap<String, (String, String)>> =
        RefCell::new(std::collections::HashMap::new());
    /// GAP 1's identity bank (`messaging::MessageBank`): `(class, bytes)`
    /// banked at every observed `serializeToString`, looked up by a
    /// `MessagingPublish` begin with no `type` header. PER-REQUEST, unlike
    /// CONSUMER_QUEUES above — a publish body only ever needs to match
    /// something serialized THIS request (a stale cross-request match would
    /// be exactly the kind of coincidence `MessageBank`'s docblock argues a
    /// short-lived, per-request scope is what keeps cheap), so it is cleared
    /// in both `set_request_context` and `clear_request_context` alongside
    /// CURL_HEADERS / PREPARED_STATEMENTS.
    static MESSAGE_BANK: RefCell<crate::messaging::MessageBank> =
        RefCell::new(crate::messaging::MessageBank::new());
}

/// Ceiling on remembered consumer subscriptions per process. A worker owns a
/// handful; 256 is a runaway backstop, not a budget. Insert always lands (the
/// map overwrites known keys and refuses only NEW keys past the cap), because a
/// re-subscribe under a recycled handle must never keep the OLD queue name.
#[cfg(feature = "zend-observer")]
const MAX_CONSUMER_QUEUES: usize = 256;

/// Ceiling on remembered NATS subscriptions per process — the `SUBSCRIBE_SIDS`
/// sibling of `MAX_CONSUMER_QUEUES` above, same reasoning: a worker owns a
/// handful of subjects, insert always lands for a KNOWN sid (client-minted
/// sids do not collide within one process), and only a brand new sid past the
/// cap is refused.
#[cfg(feature = "zend-observer")]
const MAX_SUBSCRIBE_SIDS: usize = 256;

/// What a message-scoped delivery request's ROUTE — the string
/// `close_messaging_delivery` hands `crate::end_request` as `http.route`, and
/// (in provisional form, see below) the name `chronos_job_started` marks the
/// delivery in-flight under — is known as, at the moment `DELIVERY_SCOPE` is
/// written.
///
///   * `Known`: Bunny and NATS. Every fact the route needs (the banked
///     subscription queue / NATS subject, the routing key / exchange
///     fallback) is a plain PHP property already populated by the time this
///     call's BEGIN handler runs — there is nothing left to discover later,
///     so the resolved `String` is carried to CLOSE unchanged.
///   * `DeferredAmqplib`: `PhpAmqpLib\Channel\AMQPChannel::basic_deliver`'s
///     envelope — consumer tag, exchange, routing key, `redelivered` — is
///     NOT yet a property on `$message` at BEGIN. It lives in `$reader`, the
///     raw AMQP method-frame buffer this call's OWN body has not parsed yet;
///     `$reader->read_shortstr()`/`read_longlong()`/`read_bit()` are the
///     FIRST statements of `basic_deliver`'s body (verified against the
///     vendored source), and calling them ourselves from the begin handler
///     would consume the exact same cursor the function's real parse is
///     about to run — corrupting the very call we are only supposed to be
///     watching. The one place these facts become safely re-readable
///     PROPERTIES is on `$message` itself, once `setDeliveryInfo()` /
///     `setConsumerTag()` (both called before `call_user_func`, so
///     unconditionally done by the time this call ends) have run.
///     `DeferredAmqplib` is a bare marker carrying nothing: resolution
///     (`resolve_amqplib_delivery_route`) re-reads arg 1 fresh off the SAME
///     `execute_data` the end trampoline is already holding, at CLOSE, when
///     those properties are finally live. A consequence stated plainly: the
///     in-flight job marker `begin_messaging_delivery_amqplib` writes is
///     named with the `"amqp"` floor, not the real queue — an operator
///     inspecting a HUNG delivery (worker killed mid-message, never
///     reaching `close_messaging_delivery`) sees the broker, not the queue,
///     until the message finishes. Narrow and structurally forced by the
///     wire-parsing order, not a missed lookup.
#[cfg(feature = "zend-observer")]
enum DeliveryRoute {
    Known(String),
    DeferredAmqplib,
}

#[cfg(feature = "zend-observer")]
impl DeliveryRoute {
    /// Resolve to the actual route string. `execute_data` is the CLOSING
    /// call's own — for a `DeferredAmqplib` scope this is always
    /// `basic_deliver`'s `execute_data`, since `owns_delivery_scope` frames
    /// close in the SAME end-trampoline invocation that popped them, never a
    /// nested one (depth-1 by construction, `DELIVERY_SCOPE`'s own docblock).
    ///
    /// # Safety
    /// Called from the Zend observer end handler with a valid `execute_data`
    /// for the call being unwound.
    unsafe fn resolve(self, execute_data: *mut ext_php_rs::ffi::zend_execute_data) -> String {
        match self {
            DeliveryRoute::Known(route) => route,
            DeliveryRoute::DeferredAmqplib => resolve_amqplib_delivery_route(execute_data),
        }
    }
}

/// amqplib's queue-name fallback chain, re-read at CLOSE (see
/// `DeliveryRoute::DeferredAmqplib`'s docblock): the queue this consumer
/// subscribed with (`CONSUMER_QUEUES`, keyed by channel handle + the now-live
/// `consumerTag` property), else the routing key, else the exchange, else
/// the `"amqp"` floor — the exact chain `begin_messaging_delivery_bunny`
/// already uses, restated for amqplib's (private, but engine-readable)
/// property names. Also merges the destination + `redelivered` facts into
/// the request-attribute bag here, since those too are only readable now —
/// `crate::end_request` (called immediately after this returns) drains that
/// bag into the root span, so a merge here still lands on the right span.
///
/// # Safety
/// Called from the Zend observer end handler with a valid `execute_data` for
/// the `basic_deliver` call being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn resolve_amqplib_delivery_route(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
) -> String {
    use crate::messaging;

    const FLOOR: &str = "amqp";

    let Some(message) = zend_helpers::arg_object(execute_data, 1) else {
        return FLOOR.to_owned();
    };
    let consumer_tag =
        zend_helpers::object_property_string(message, "consumerTag", 256).unwrap_or_default();
    let channel_handle = zend_helpers::this_object_handle(execute_data);
    let queue = match (channel_handle, consumer_tag.is_empty()) {
        (Some(handle), false) => CONSUMER_QUEUES.with(|map| {
            map.borrow()
                .get(&(handle, consumer_tag.clone()))
                .cloned()
                .unwrap_or_default()
        }),
        _ => String::new(),
    };
    let routing_key = zend_helpers::object_property_string(message, "routingKey", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let exchange = zend_helpers::object_property_string(message, "exchange", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let redelivered =
        zend_helpers::object_property_bool(message, "redelivered").unwrap_or(false);

    // vhost: readable at BEGIN too (the connection is already live by
    // `basic_deliver`-entry — the contract's own "No gap" note), but re-read
    // here rather than threaded through `DELIVERY_SCOPE` — one less thing
    // that marker needs to carry, and this runs once per delivery regardless.
    let channel = zend_helpers::this_object(execute_data);
    let connection = channel.and_then(|c| zend_helpers::object_property_object(c, "connection"));
    let vhost = connection
        .and_then(|c| zend_helpers::object_property_string(c, "vhost", 256))
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();

    let destination = messaging::amqp_destination(&vhost, &exchange, &routing_key, &queue);
    let mut facts: Vec<(String, String)> = destination
        .attributes()
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    if redelivered {
        facts.push(("messaging.message.redelivered".into(), "true".into()));
    }
    crate::request_attributes::merge(facts);

    if !queue.is_empty() {
        queue
    } else if !routing_key.is_empty() {
        routing_key
    } else if !exchange.is_empty() {
        exchange
    } else {
        FLOOR.to_owned()
    }
}

/// Which broker's END-time retval shape a banked subscribe function needs —
/// see `CONSUME_FNS`'s docblock.
#[cfg(feature = "zend-observer")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConsumeSubscribeKind {
    /// `Bunny\Channel::consume`: retval is a `MethodBasicConsumeOkFrame`
    /// OBJECT with a public `consumerTag` property.
    Bunny,
    /// `PhpAmqpLib\Channel\AMQPChannel::basic_consume`: retval is the final
    /// consumer tag as a plain STRING directly (server-assigned when the
    /// caller passed `''`) — no wrapper object at all.
    Amqplib,
}

/// Bank one successful subscribe call's `(channel, consumerTag) → queue`.
/// Called from the end trampoline for EVERY observed call; the lookup
/// against `CONSUME_FNS` rejects everything that is not a banked subscribe
/// function in one small linear scan, then dispatches to whichever broker's
/// retval shape that function needs.
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are
/// both still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn maybe_bank_consumer_queue(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
) {
    let function = zend_helpers::function_ptr(execute_data);
    if function == 0 {
        return;
    }
    let kind = CONSUME_FNS.with(|fns| {
        fns.borrow()
            .iter()
            .find(|(known, _)| *known == function)
            .map(|(_, kind)| *kind)
    });
    match kind {
        Some(ConsumeSubscribeKind::Bunny) => maybe_bank_bunny_consumer_queue(execute_data, retval),
        Some(ConsumeSubscribeKind::Amqplib) => {
            maybe_bank_amqplib_consumer_queue(execute_data, retval)
        }
        None => {}
    }
}

/// Bank one successful `Bunny\Channel::consume`'s
/// `(channel, consumerTag) → queue`.
///
/// The AsyncClient path returns a Promise rather than a
/// `MethodBasicConsumeOkFrame`; a Promise has no `consumerTag` property, the
/// read yields nothing and nothing is banked — naming then degrades to routing
/// key / exchange at dispatch time, honestly (contract §6.5).
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are
/// both still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn maybe_bank_bunny_consumer_queue(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
) {
    let Some(channel) = zend_helpers::this_object_handle(execute_data) else {
        return;
    };
    let Some(ok_frame) = zend_helpers::retval_object(retval) else {
        return;
    };
    // `MethodBasicConsumeOkFrame::$consumerTag` is public; a Promise (async
    // subscribe) has no such property and reads as None.
    let Some(tag) = zend_helpers::object_property_string(ok_frame, "consumerTag", 256) else {
        return;
    };
    if tag.is_empty() {
        return;
    }
    // Channel::consume(callable $callback, $queue = "", ...): arg 1 is the queue
    // the application asked for. A server-named queue ("" — the app let the
    // broker pick) banks nothing; the dispatch fallback names the delivery from
    // its routing key instead of recording an empty name.
    let queue = zend_helpers::arg_scalar_string(execute_data, 1, 512).unwrap_or_default();
    let queue = queue.trim().to_owned();
    if queue.is_empty() {
        return;
    }
    bank_consumer_queue(channel, tag, queue);
}

/// Bank one successful `PhpAmqpLib\Channel\AMQPChannel::basic_consume`'s
/// `(channel, consumerTag) → queue` — the amqplib contract's "Consume
/// scoping" section. Unlike Bunny's, the retval here is the final consumer
/// tag as a plain STRING directly (verified against the vendored source,
/// `AMQPChannel::basic_consume`: `return $consumer_tag;`, itself either the
/// caller's own arg 1 or the server-assigned replacement read off the
/// `basic.consume_ok` wait when the caller passed `''`) — no wrapper object,
/// so no property read is needed at all.
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are
/// both still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn maybe_bank_amqplib_consumer_queue(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
) {
    let Some(channel) = zend_helpers::this_object_handle(execute_data) else {
        return;
    };
    let Some(tag) = zend_helpers::scalar_to_string(retval).map(|s| s.trim().to_owned()) else {
        return;
    };
    if tag.is_empty() {
        return;
    }
    // basic_consume($queue = '', ...): arg 0 is the queue the application
    // asked for. A server-named queue banks nothing, same honest degrade as
    // Bunny's.
    let queue = zend_helpers::arg_scalar_string(execute_data, 0, 512).unwrap_or_default();
    let queue = queue.trim().to_owned();
    if queue.is_empty() {
        return;
    }
    bank_consumer_queue(channel, tag, queue);
}

/// The shared `CONSUMER_QUEUES` insert both banking functions above end
/// with — same cap, same insert-overwrites-known-keys rule, one place to
/// read instead of two copies drifting apart.
#[cfg(feature = "zend-observer")]
fn bank_consumer_queue(channel: u32, consumer_tag: String, queue: String) {
    CONSUMER_QUEUES.with(|map| {
        let mut map = map.borrow_mut();
        let key = (channel, consumer_tag);
        if map.len() >= MAX_CONSUMER_QUEUES && !map.contains_key(&key) {
            return;
        }
        map.insert(key, queue);
    });
}

/// GAP 1: bank one serialized protobuf message's identity —
/// `Google\Protobuf\Internal\Message::serializeToString`'s end handler, called
/// for EVERY observed call (the name compare in the end trampoline rejects
/// everything else in one string comparison, the same shape as
/// `maybe_bank_consumer_queue` above).
///
/// `$this` is read for its RUNTIME class (`object_class_name`, NOT
/// `frame.name`'s declaring scope — see that helper's docblock for why the
/// two differ for exactly this method) because that IS the schema type the
/// registry needs: a generated `QlsProtocol\Shared\Webhook` never overrides
/// `serializeToString`, so the function identity alone can only ever say
/// "some `Message`". The RETURN VALUE is the serialized bytes about to leave
/// the process — reading it here, at the moment it exists, is what makes this
/// a zero-application-code seam: no app code, no SDK, no bespoke publisher
/// wrapper had to change for the registry to learn the class again.
///
/// READ-ONLY: no zval is written here, unlike the publish path's header
/// injection. A miss at any step (no `$this`, an unreadable class, a
/// non-string or empty return) simply banks nothing — the later publish
/// lookup already treats "nothing banked" as an absent name, identical to
/// today's behaviour for an app that sets no `type` header.
///
/// # Safety
/// Called from the Zend observer end handler with a valid `execute_data` and
/// `retval` for the call being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn bank_serialized_message(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
) {
    let Some(object) = zend_helpers::this_object(execute_data) else {
        return;
    };
    let Some(class) = zend_helpers::object_class_name(object) else {
        return;
    };
    let Some(bytes) = zend_helpers::retval_string_bytes(retval) else {
        return;
    };
    MESSAGE_BANK.with(|bank| bank.borrow_mut().record(&class, &bytes));
}

/// Wall-clock epoch seconds — the messaging paths' only wall reading. A wall
/// clock on purpose, despite being the worse clock: the enqueued-at stamp is
/// compared by ANOTHER process on another machine, so the only comparable
/// instant is the one both machines claim about the same world (see
/// `MessagingWait`'s reasoning, ported in `messaging.rs`).
#[cfg(feature = "zend-observer")]
fn epoch_seconds_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// One line per process per REASON when header injection degrades — the
/// spool_log/heartbeat latch idiom. The consequence is stated because it is
/// subtle: the span is still recorded (a recorded span whose id is not on the
/// wire breaks nothing — the causality promise runs the other way), but the
/// consumer of this message will root a fresh trace. A fabricated id is never
/// placed on the wire by any fallback.
#[cfg(feature = "zend-observer")]
fn warn_injection_failure(reason: &'static str) {
    warn_injection_failure_for("Bunny\\AbstractClient::publish", reason);
}

/// The broker-parameterised form every messaging publish path (Bunny,
/// RdKafka, amqplib, NATS) now shares — one line per process per (method,
/// reason) PAIR, since the three brokers' injectors fail for different
/// reasons under different names and collapsing them onto one latch set
/// would silently swallow a second broker's first warning if it happened to
/// reuse a reason string an earlier broker already latched.
fn warn_injection_failure_for(method: &'static str, reason: &'static str) {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<(&'static str, &'static str)>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut seen = match seen.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if seen.insert((method, reason)) {
        eprintln!(
            "chronos-php: traceparent not injected into {method} \
             ({reason}); publish span recorded, its consumer will start a new trace"
        );
    }
}

/// The exact pass-through the ObserveOnly branch of the begin trampoline runs,
/// as a function the messaging handlers can fall back to on ANY path that
/// declines to act — so a declined publish/delivery keeps begin/end pairing
/// (and the deterministic aggregate's balance) byte-identical to an unlisted
/// userland call.
#[cfg(feature = "zend-observer")]
unsafe fn observe_only_fallback(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
) {
    let has_context = REQUEST_CONTEXT.with(|ctx| ctx.borrow().is_some());
    if has_context || crate::dst_spool::is_active() {
        let arguments = capture_tier_three_arguments(execute_data, interned);
        let defining_file = if interned.internal {
            None
        } else {
            interned.file()
        };
        record_call_path_enter(name, interned.internal, defining_file, &arguments);
        push_frame(CallFrame::observe_only(name.clone()), interned.id);
    }
}

/// PUBLISH observation for `MESSAGING_PUBLISH_METHODS` — producer span, the
/// `MessagingDestination` vocabulary, and native W3C header injection, replacing
/// the estates' bespoke publish shims with zero application changes.
///
/// Ordering inside is load-bearing:
///
///   1. Facts are read off the ORIGINAL argument list first — injection may
///      copy-on-write the headers array, and the caller-wins rule means the
///      caller's own `content-type`/`type` are the values that matter either way
///      (native never writes those keys: they are application declarations).
///   2. The frame is minted (`on_begin`) BEFORE the injection so the span id
///      that goes on the wire is the id that will be recorded at end — the
///      causality contract, satisfied the way the curl path proved.
///   3. Injection runs OUTSIDE the request-context gate: the enqueued-at stamp
///      rides EVERY publish, including one with no open request (a scheduled
///      command's message has waited just as long). The trace headers, and the
///      span itself, still require an open context.
///
/// Everything fail-open: any unreadable fact is an absent attribute, an
/// injection failure is one warn-once line, and nothing here can change what
/// the application publishes.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid execute_data.
#[cfg(feature = "zend-observer")]
unsafe fn begin_messaging_publish_bunny(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
) {
    use crate::messaging;

    // Args at the AbstractClient::publish frame: 0=$channel(int), 1=$body,
    // 2=$headers(array), 3=$exchange, 4=$routingKey. `$this` is the client.
    let exchange = zend_helpers::arg_scalar_string(execute_data, 3, 512).unwrap_or_default();
    let routing_key = zend_helpers::arg_scalar_string(execute_data, 4, 512).unwrap_or_default();
    // The caller's own declarations, read from the headers argument. After the
    // caller-wins merge these are unchanged by construction — native adds
    // neither key — so reading before injection is reading the merged truth.
    let content_type = zend_helpers::arg_array_str_key_string(execute_data, 2, "content-type", 256)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    // The AMQP `type` property: the PRIMARY seam for the DTO class name
    // (`$headers['type'] = Webhook::class` is application data, not telemetry
    // code), landing as `messaging.message.name` on BOTH halves natively.
    let message_name = zend_helpers::arg_array_str_key_string(execute_data, 2, "type", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    // RAW bytes, not a lossy String: the encode decision (text vs base64) must
    // see the payload as published or a protobuf body is corrupted before it.
    let body = zend_helpers::arg_string_bytes(execute_data, 1);
    // GAP 1's FALLBACK: this estate's publishers never set `type`, so the
    // primary seam above is empty for every message. Recover the class from
    // the `serializeToString` bank (`bank_serialized_message`) by looking up
    // THIS EXACT body's identity. A miss (never serialized through the
    // observed method — e.g. a hand-built string body) or an ambiguous
    // identity (`MessageBank::record` poisons any identity two DIFFERENT
    // classes touch this request) both fall through to the same absent name
    // an app with no `type` header already gets: this fallback can only ever
    // ADD a name, never invent a wrong one.
    let message_name = if !message_name.is_empty() {
        message_name
    } else {
        body.as_deref()
            .and_then(|bytes| MESSAGE_BANK.with(|bank| bank.borrow().lookup(bytes).map(str::to_owned)))
            .unwrap_or_default()
    };

    let context = REQUEST_CONTEXT.with(|ctx| ctx.borrow().as_ref().cloned());
    let frame = context
        .as_ref()
        .map(|ctx| on_begin(ctx, name, SpanPolicy::MessagingPublish));

    // Injection. Add-if-absent per key (`inject_array_entries`): a caller's own
    // traceparent/tracestate/baggage/stamp always wins, never validated, never
    // rewritten.
    let mut entries: Vec<(&str, String)> = Vec::new();
    if let Some(frame) = &frame {
        if let Some(traceparent) = &frame.injected_traceparent {
            entries.push(("traceparent", traceparent.clone()));
        }
    }
    if let Some(ctx) = &context {
        // Forward-as-is, verbatim — the same W3C contract
        // `merge_propagation_headers` documents for curl.
        if let Some(tracestate) = &ctx.tracestate {
            entries.push(("tracestate", tracestate.clone()));
        }
        if let Some(baggage) = &ctx.baggage {
            entries.push(("baggage", baggage.clone()));
        }
    }
    entries.push((
        messaging::ENQUEUED_AT_HEADER,
        messaging::enqueued_at_stamp(epoch_seconds_now()),
    ));
    // THE GAP's fix: hand the CONSUME side the same class identity this span
    // just resolved (`type` property or the `serializeToString` bank above) —
    // whichever it is, it is never a guess (an empty `message_name` here means
    // neither source yielded one, so nothing is injected: absent, not
    // fabricated). Add-if-absent like every other entry, so an application
    // that already sets this exact header keeps its own value.
    if !message_name.is_empty() {
        entries.push((messaging::SCHEMA_HEADER, message_name.clone()));
    }
    // BUG FIX (RdKafka contract): `bypass_span_cap` must reflect whether a
    // promise actually went on the wire, not merely "this policy is
    // MessagingPublish" — Bunny's chain always passes all 7 args positionally
    // (see this function's own docblock), so injection here always has a real
    // slot to write into; `inject_array_entries`'s `Ok`/`Err` is still the
    // single source of truth, so a future change to Bunny's call shape (or an
    // application calling `Client::publish` directly with defaults) degrades
    // this correctly rather than silently keeping a stale `true`.
    let injection = zend_helpers::inject_array_entries(execute_data, 2, &entries);
    let propagated = injection.is_ok();
    if let Err(reason) = injection {
        warn_injection_failure(reason);
    }

    let Some(mut frame) = frame else {
        // No open request: the stamp is on the wire (that is all a scheduled
        // command's publish gets), and there is no span to record. Keep the
        // frame pairing exactly as ObserveOnly would.
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    frame.bypass_span_cap = propagated;

    // The vocabulary, every empty value dropped — absent, never guessed.
    // `$this->options` is protected with no getter; native property reads see
    // protected members, which is what finally puts `server.address` on a
    // publish span (the fact the bespoke bridge could never reach, and the
    // reason the service map's broker node read "broker not identified").
    let client = zend_helpers::this_object(execute_data);
    let read_option = |key: &str| {
        client
            .and_then(|object| {
                zend_helpers::object_property_table_string(object, "options", key, 256)
            })
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    let vhost = read_option("vhost");
    let host = read_option("host");
    let port = read_option("port");

    // Queue unknown at publish: the routing key becomes NAME only on the default
    // exchange (forAmqp's rule); a named exchange leaves NAME absent.
    let destination = messaging::amqp_destination(&vhost, &exchange, &routing_key, "");
    frame.span_name = Some(messaging::publish_label(&destination));

    frame.attributes.push(("span.kind".into(), "producer".into()));
    frame
        .attributes
        .push(("messaging.system".into(), messaging::SYSTEM_RABBITMQ.into()));
    frame
        .attributes
        .push(("messaging.operation".into(), "publish".into()));
    for (key, value) in destination.attributes() {
        frame.attributes.push((key.into(), value));
    }
    if !host.is_empty() {
        frame.attributes.push(("server.address".into(), host));
    }
    if !port.is_empty() {
        frame.attributes.push(("server.port".into(), port));
    }
    if let Some(word) = messaging::protocol(&content_type) {
        frame
            .attributes
            .push(("messaging.protocol".into(), word.into()));
    }
    if !content_type.is_empty() {
        frame.attributes.push((
            "messaging.message.body.content_type".into(),
            content_type.clone(),
        ));
    }
    if !message_name.is_empty() {
        frame
            .attributes
            .push(("messaging.message.name".into(), message_name));
    }
    if let Some(bytes) = &body {
        // Size always, even with capture off — measured on the raw wire bytes
        // before any cap or base64, the one payload fact that is free.
        frame
            .attributes
            .push(("messaging.message.body.size".into(), bytes.len().to_string()));
        if messaging::capture_enabled() {
            let preview_cap = messaging::PUBLISH_PREVIEW_CAP.min(messaging::body_ceiling());
            if let Some(encoded) = messaging::encode_body(bytes, preview_cap) {
                frame
                    .attributes
                    .push(("messaging.message.body".into(), encoded.body));
                if encoded.base64 {
                    frame
                        .attributes
                        .push(("messaging.message.body.encoding".into(), "base64".into()));
                }
                if encoded.truncated {
                    frame
                        .attributes
                        .push(("messaging.message.body.truncated".into(), "true".into()));
                }
            }
            // The whole copy, keyed by THIS frame's ids — the publish span is
            // the span that previews it. `.stored` only when the store took the
            // bytes: the marker never out-promises the store.
            if let Some((payload, encoding)) = messaging::whole_body(
                bytes,
                messaging::PUBLISH_PREVIEW_CAP,
                messaging::body_ceiling(),
            ) {
                if crate::store_message_body(
                    frame.trace_id.clone(),
                    frame.span_id.clone(),
                    content_type.clone(),
                    payload,
                    encoding.to_owned(),
                ) {
                    frame
                        .attributes
                        .push(("messaging.message.body.stored".into(), "true".into()));
                }
            }
        }
    }
    if let Some(file) = interned.file() {
        frame
            .attributes
            .push(("code.filepath".into(), file.to_owned()));
    }

    let arguments = capture_tier_three_arguments(execute_data, interned);
    record_call_path_enter(name, interned.internal, interned.file(), &arguments);
    push_frame(frame, interned.id);
}

/// PUBLISH observation for RdKafka's two producer methods — the RdKafka
/// observation contract in full. `$this` is the `ProducerTopic`; both
/// `produce`/`producev` share this one begin handler, branching ONLY on
/// whether a headers argument exists to inject into (`producev`, index 4) —
/// `produce` has no headers parameter AT ALL, not merely an omitted one, so
/// it is span-only by construction and this function never attempts
/// injection for it. `is_internal` is true for both (C-extension
/// `PHP_METHOD`s), which is why `MESSAGING_PUBLISH_METHODS` is checked ahead
/// of the `is_internal` early return in `observe_policy` — see that table's
/// own docblock.
///
/// Args: 0=`$partition`(int), 1=`$msgflags`(int), 2=`$payload`(?string),
/// 3=`$key`(?string), and for `producev` only: 4=`$headers`(?array),
/// 5=`$timestamp_ms`, 6=`$msg_opaque`.
///
/// The topic NAME needs a native→PHP call-back (`ProducerTopic::getName()`,
/// via `call_object_method_string`): it is neither an argument nor a
/// declared property (`kafka_topic_object`'s C struct field is invisible to
/// `zend_read_property`) — see that helper's own docblock for why this is
/// safe re-entrancy, the same shape `curl_effective_url` already uses.
///
/// `messaging.message.name` has only ONE source for Kafka (never a `type`
/// property — Kafka has no AMQP-`type` equivalent): the `serializeToString`
/// bank (`MESSAGE_BANK`), looked up by the exact same body-identity mechanism
/// Bunny's own fallback uses — `resolve_message_name` is reused unchanged
/// with an always-empty `type_header`.
///
/// `server.address`/cluster/`messaging.kafka.consumer.group` are ALWAYS
/// absent for RdKafka (v1 decision, contract's "vhost/cluster/server.address
/// recovery" section) — no Conf key is ever visible as a zend property, and
/// reaching them needs either a new librdkafka C dependency or a 3-hop
/// `Conf::set`/`newTopic` handle-keyed side-channel bank; both are listed as
/// v2 follow-ups, not built here.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid execute_data.
#[cfg(feature = "zend-observer")]
unsafe fn begin_messaging_publish_rdkafka(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
) {
    use crate::messaging;

    let is_producev = &**name == "RdKafka\\ProducerTopic::producev";
    let partition = zend_helpers::arg_long(execute_data, 0);
    let key = zend_helpers::arg_scalar_string(execute_data, 3, 256);
    // RAW bytes: the encode decision (text vs base64) must see the payload as
    // published, not a lossy UTF-8 conversion of it.
    let body = zend_helpers::arg_string_bytes(execute_data, 2);
    // GAP 1's ONLY source for Kafka: no AMQP `type` property exists on this
    // wire format at all, so `resolve_message_name`'s "primary seam" is
    // always empty here — the bank lookup is the whole story.
    let message_name = body
        .as_deref()
        .and_then(|bytes| MESSAGE_BANK.with(|bank| bank.borrow().lookup(bytes).map(str::to_owned)))
        .unwrap_or_default();

    let context = REQUEST_CONTEXT.with(|ctx| ctx.borrow().as_ref().cloned());
    let frame = context
        .as_ref()
        .map(|ctx| on_begin(ctx, name, SpanPolicy::MessagingPublish));

    // Injection: `producev` only — `produce` has no headers slot to write
    // into at all, so nothing is ever attempted for it (the honest
    // consequence the contract states plainly: an estate on `produce()`
    // gets zero native propagation of any kind, not even the enqueued-at
    // stamp, because there is no wire carrier).
    let propagated = if is_producev {
        let mut entries: Vec<(&str, String)> = Vec::new();
        if let Some(frame) = &frame {
            if let Some(traceparent) = &frame.injected_traceparent {
                entries.push(("traceparent", traceparent.clone()));
            }
        }
        if let Some(ctx) = &context {
            if let Some(tracestate) = &ctx.tracestate {
                entries.push(("tracestate", tracestate.clone()));
            }
            if let Some(baggage) = &ctx.baggage {
                entries.push(("baggage", baggage.clone()));
            }
        }
        entries.push((
            messaging::ENQUEUED_AT_HEADER,
            messaging::enqueued_at_stamp(epoch_seconds_now()),
        ));
        if !message_name.is_empty() {
            entries.push((messaging::SCHEMA_HEADER, message_name.clone()));
        }
        let injection = zend_helpers::inject_array_entries(execute_data, 4, &entries);
        let ok = injection.is_ok();
        if let Err(reason) = injection {
            warn_injection_failure_for("RdKafka\\ProducerTopic::producev", reason);
        }
        ok
    } else {
        false
    };

    let Some(mut frame) = frame else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    frame.bypass_span_cap = propagated;

    // The topic name: the call-back bridge, since it is reachable no other
    // way (see this function's own docblock).
    let topic = zend_helpers::this_object(execute_data)
        .and_then(|topic| zend_helpers::call_object_method_string(topic, "getName", 512))
        .unwrap_or_default();
    let destination = messaging::kafka_destination(&topic);
    frame.span_name = Some(messaging::labeled("PUBLISH", &destination, messaging::SYSTEM_KAFKA));

    frame.attributes.push(("span.kind".into(), "producer".into()));
    frame
        .attributes
        .push(("messaging.system".into(), messaging::SYSTEM_KAFKA.into()));
    frame
        .attributes
        .push(("messaging.operation".into(), "publish".into()));
    for (key, value) in destination.attributes() {
        frame.attributes.push((key.into(), value));
    }
    // Kafka-specific, informational, never fed into Destination — partition
    // is explicitly not identity (see `kafka_partition_attribute`'s
    // docblock).
    if let Some(partition) = partition.and_then(messaging::kafka_partition_attribute) {
        frame
            .attributes
            .push(("messaging.kafka.destination.partition".into(), partition));
    }
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        frame.attributes.push(("messaging.kafka.message.key".into(), key));
    }
    if !message_name.is_empty() {
        frame
            .attributes
            .push(("messaging.message.name".into(), message_name));
    }
    if let Some(bytes) = &body {
        frame
            .attributes
            .push(("messaging.message.body.size".into(), bytes.len().to_string()));
        if messaging::capture_enabled() {
            let preview_cap = messaging::PUBLISH_PREVIEW_CAP.min(messaging::body_ceiling());
            if let Some(encoded) = messaging::encode_body(bytes, preview_cap) {
                frame
                    .attributes
                    .push(("messaging.message.body".into(), encoded.body));
                if encoded.base64 {
                    frame
                        .attributes
                        .push(("messaging.message.body.encoding".into(), "base64".into()));
                }
                if encoded.truncated {
                    frame
                        .attributes
                        .push(("messaging.message.body.truncated".into(), "true".into()));
                }
            }
            if let Some((payload, encoding)) = messaging::whole_body(
                bytes,
                messaging::PUBLISH_PREVIEW_CAP,
                messaging::body_ceiling(),
            ) {
                if crate::store_message_body(
                    frame.trace_id.clone(),
                    frame.span_id.clone(),
                    String::new(),
                    payload,
                    encoding.to_owned(),
                ) {
                    frame
                        .attributes
                        .push(("messaging.message.body.stored".into(), "true".into()));
                }
            }
        }
    }
    if let Some(file) = interned.file() {
        frame
            .attributes
            .push(("code.filepath".into(), file.to_owned()));
    }

    let arguments = capture_tier_three_arguments(execute_data, interned);
    record_call_path_enter(name, interned.internal, interned.file(), &arguments);
    push_frame(frame, interned.id);
}

/// PUBLISH observation for `PhpAmqpLib\Channel\AMQPChannel::basic_publish` —
/// the amqplib observation contract in full.
///
/// Args: 0=`$msg` (`AMQPMessage`), 1=`$exchange`, 2=`$routing_key`,
/// 3=`$mandatory`, 4=`$immediate`, 5=`$ticket`. `$this` is the channel.
///
/// Injection target: `$msg->properties['application_headers']`, via
/// `inject_amqp_application_headers` — see that function's docblock for the
/// three-shape branching (absent / plain array / `AMQPTable` object), all
/// protected-property writes, none a userland method call. This CORRECTS the
/// premise of the old in-tree follow-up note (an `AMQPTable` object was
/// assumed to require a method call) — verified against the vendored source:
/// neither `AMQPMessage::$properties` nor `AMQPTable::$data` enforce a
/// runtime type, so a raw array is exactly as legal a wire value as a real
/// `AMQPTable`.
///
/// `server.address`/`server.port`/vhost: a three-hop protected-property chain
/// off `$this` (`channel->connection->io->host/port`,
/// `channel->connection->vhost`) — `checkConnection()` (called at the very
/// top of `basic_publish`, before this begin handler even runs, since begin
/// fires at frame entry which is AFTER `$this->checkConnection()`'s own
/// frame has already returned... texture note: `checkConnection` is called
/// FROM `basic_publish`'s own body, so by the time OUR begin handler for
/// `basic_publish` fires — at `basic_publish`'s OWN entry, before its body
/// runs — the lazy connect has NOT necessarily happened yet for a fresh
/// `AMQPLazyConnection`. Read anyway: `io` reads as absent until the connect
/// completes, which just means the span degrades to no `server.address`
/// rather than a stale/wrong one — no different from any other "unreadable
/// fact is an absent attribute" case elsewhere in this file.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid execute_data.
#[cfg(feature = "zend-observer")]
unsafe fn begin_messaging_publish_amqplib(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
) {
    use crate::messaging;

    let Some(msg) = zend_helpers::arg_object(execute_data, 0) else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    let exchange = zend_helpers::arg_scalar_string(execute_data, 1, 512).unwrap_or_default();
    let routing_key = zend_helpers::arg_scalar_string(execute_data, 2, 512).unwrap_or_default();
    // The caller's own declarations, read BEFORE injection — after the
    // caller-wins merge these are unchanged by construction, so reading them
    // now is reading the merged truth either way. `content_type`/`type` are
    // PLAIN scalar keys directly in `$msg->properties` (protocolWriter's
    // `$propertyDefinitions` lists them as siblings of `application_headers`,
    // not nested under it) — a straight property-table read, the same shape
    // Bunny's `$this->options['vhost']` read already uses; only the actual
    // HEADER key/value pairs (`traceparent`, `x-chronos-schema`, …) live one
    // level deeper, under `application_headers`, which is what
    // `amqp_application_header` is for.
    let content_type = zend_helpers::object_property_table_string(msg, "properties", "content_type", 256)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let message_name = zend_helpers::object_property_table_string(msg, "properties", "type", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    // `$msg->body` is a public, non-deprecated-for-reading string property
    // (`AMQPMessage::$body`, populated by `setBody()`), read as RAW bytes so
    // the text-or-base64 decision sees the payload as it will be published.
    let body = zend_helpers::object_property_bytes(msg, "body");
    // GAP 1's fallback: an estate that never sets `type` recovers the class
    // from the `serializeToString` bank, identical mechanism to Bunny's.
    let message_name = if !message_name.is_empty() {
        message_name
    } else {
        body.as_deref()
            .and_then(|bytes| MESSAGE_BANK.with(|bank| bank.borrow().lookup(bytes).map(str::to_owned)))
            .unwrap_or_default()
    };

    let context = REQUEST_CONTEXT.with(|ctx| ctx.borrow().as_ref().cloned());
    let frame = context
        .as_ref()
        .map(|ctx| on_begin(ctx, name, SpanPolicy::MessagingPublish));

    let mut entries: Vec<(&str, String)> = Vec::new();
    if let Some(frame) = &frame {
        if let Some(traceparent) = &frame.injected_traceparent {
            entries.push(("traceparent", traceparent.clone()));
        }
    }
    if let Some(ctx) = &context {
        if let Some(tracestate) = &ctx.tracestate {
            entries.push(("tracestate", tracestate.clone()));
        }
        if let Some(baggage) = &ctx.baggage {
            entries.push(("baggage", baggage.clone()));
        }
    }
    entries.push((
        messaging::ENQUEUED_AT_HEADER,
        messaging::enqueued_at_stamp(epoch_seconds_now()),
    ));
    if !message_name.is_empty() {
        entries.push((messaging::SCHEMA_HEADER, message_name.clone()));
    }
    let injection = zend_helpers::inject_amqp_application_headers(msg, &entries);
    let propagated = injection.is_ok();
    if let Err(reason) = injection {
        warn_injection_failure_for("PhpAmqpLib\\Channel\\AMQPChannel::basic_publish", reason);
    }

    let Some(mut frame) = frame else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    frame.bypass_span_cap = propagated;

    // vhost/server.address/server.port: the three-hop protected chain off
    // `$this` (the channel) — see this function's own docblock.
    let channel = zend_helpers::this_object(execute_data);
    let connection = channel.and_then(|c| zend_helpers::object_property_object(c, "connection"));
    let vhost = connection
        .and_then(|c| zend_helpers::object_property_string(c, "vhost", 256))
        .map(|v| v.trim().to_owned())
        .unwrap_or_default();
    let io = connection.and_then(|c| zend_helpers::object_property_object(c, "io"));
    let host = io
        .and_then(|io| zend_helpers::object_property_string(io, "host", 256))
        .map(|v| v.trim().to_owned())
        .unwrap_or_default();
    let port = io
        .and_then(|io| zend_helpers::object_property_string(io, "port", 32))
        .map(|v| v.trim().to_owned())
        .unwrap_or_default();

    // Queue unknown at publish: the routing key becomes NAME only on the
    // default exchange (forAmqp's rule); a named exchange leaves NAME absent
    // — identical vocabulary to Bunny's, this client speaks the same broker.
    let destination = messaging::amqp_destination(&vhost, &exchange, &routing_key, "");
    frame.span_name = Some(messaging::publish_label(&destination));

    frame.attributes.push(("span.kind".into(), "producer".into()));
    frame
        .attributes
        .push(("messaging.system".into(), messaging::SYSTEM_RABBITMQ.into()));
    frame
        .attributes
        .push(("messaging.operation".into(), "publish".into()));
    for (key, value) in destination.attributes() {
        frame.attributes.push((key.into(), value));
    }
    if !host.is_empty() {
        frame.attributes.push(("server.address".into(), host));
    }
    if !port.is_empty() {
        frame.attributes.push(("server.port".into(), port));
    }
    if let Some(word) = messaging::protocol(&content_type) {
        frame
            .attributes
            .push(("messaging.protocol".into(), word.into()));
    }
    if !content_type.is_empty() {
        frame.attributes.push((
            "messaging.message.body.content_type".into(),
            content_type.clone(),
        ));
    }
    if !message_name.is_empty() {
        frame
            .attributes
            .push(("messaging.message.name".into(), message_name));
    }
    if let Some(bytes) = &body {
        frame
            .attributes
            .push(("messaging.message.body.size".into(), bytes.len().to_string()));
        if messaging::capture_enabled() {
            let preview_cap = messaging::PUBLISH_PREVIEW_CAP.min(messaging::body_ceiling());
            if let Some(encoded) = messaging::encode_body(bytes, preview_cap) {
                frame
                    .attributes
                    .push(("messaging.message.body".into(), encoded.body));
                if encoded.base64 {
                    frame
                        .attributes
                        .push(("messaging.message.body.encoding".into(), "base64".into()));
                }
                if encoded.truncated {
                    frame
                        .attributes
                        .push(("messaging.message.body.truncated".into(), "true".into()));
                }
            }
            if let Some((payload, encoding)) = messaging::whole_body(
                bytes,
                messaging::PUBLISH_PREVIEW_CAP,
                messaging::body_ceiling(),
            ) {
                if crate::store_message_body(
                    frame.trace_id.clone(),
                    frame.span_id.clone(),
                    content_type.clone(),
                    payload,
                    encoding.to_owned(),
                ) {
                    frame
                        .attributes
                        .push(("messaging.message.body.stored".into(), "true".into()));
                }
            }
        }
    }
    if let Some(file) = interned.file() {
        frame
            .attributes
            .push(("code.filepath".into(), file.to_owned()));
    }

    let arguments = capture_tier_three_arguments(execute_data, interned);
    record_call_path_enter(name, interned.internal, interned.file(), &arguments);
    push_frame(frame, interned.id);
}

/// CONSUME scoping for `MESSAGING_CONSUME_DISPATCH` — Bunny's own dispatch of a
/// delivery to the callback is the only stable native scope boundary a raw AMQP
/// consumer has, and this begin turns it into a message-scoped REQUEST: the
/// native replacement for `BunnyTelemetry::consumer`'s wrapper, with no
/// application code at all.
///
/// The pass-through rules, in check order:
///
///   1. Already-active guard: a request is open, so this delivery is being
///      pumped INSIDE something already traced (a `Channel::get()` during a web
///      request, a nested event-loop run, or a consumer process running with
///      CHRONOS_PHP_CLI_ENABLED=1 whose RINIT request swallows everything —
///      keep that flag off for workers, the doctrine `BunnyTelemetry` always
///      documented). This guard is also what makes nested dispatches depth-1 by
///      construction.
///   2. Bunny's own branch guards, mirrored: only a `deliverFrame` with no
///      `returnFrame` and a still-registered `deliverCallbacks[consumerTag]`
///      reaches the callback (Channel.php:729-743) — a message Bunny would drop
///      opens no request. The `getOkFrame` branch (a `Channel::get()` pull) is
///      deliberately out of scope: its result resolves into whatever request is
///      already running.
///   3. The collector declining the request (unsampled has a context and still
///      collects; declining means disabled/no envelope) leaves the delivery
///      untraced.
///
/// Interaction with the PHP bridge (`BunnyTelemetry::consumer`): native wins by
/// ordering — this begin fires strictly before any userland wrapper runs, the
/// wrapper's own `NativeExtension::active()` guard then sees an open request and
/// passes through, and it never closes a request it did not open. Nothing
/// double-opens, nothing double-closes.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid execute_data.
#[cfg(feature = "zend-observer")]
unsafe fn begin_messaging_delivery_bunny(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
) {
    use crate::messaging;

    if crate::chronos_request_active() {
        observe_only_fallback(execute_data, name, interned);
        return;
    }
    let Some(channel) = zend_helpers::this_object(execute_data) else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    let Some(deliver_frame) = zend_helpers::object_property_object(channel, "deliverFrame") else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    if zend_helpers::object_property_object(channel, "returnFrame").is_some() {
        observe_only_fallback(execute_data, name, interned);
        return;
    }
    let consumer_tag = zend_helpers::object_property_string(deliver_frame, "consumerTag", 256)
        .unwrap_or_default();
    if consumer_tag.is_empty()
        || !zend_helpers::object_property_array_has_str_key(
            channel,
            "deliverCallbacks",
            &consumer_tag,
        )
    {
        observe_only_fallback(execute_data, name, interned);
        return;
    }

    // Naming: the banked subscription queue first (`routeName()`'s exact
    // fallback chain after it — routing key, exchange, the "amqp" floor).
    let channel_handle = (*channel).handle;
    let queue = CONSUMER_QUEUES.with(|map| {
        map.borrow()
            .get(&(channel_handle, consumer_tag.clone()))
            .cloned()
            .unwrap_or_default()
    });
    let routing_key = zend_helpers::object_property_string(deliver_frame, "routingKey", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let exchange = zend_helpers::object_property_string(deliver_frame, "exchange", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let redelivered =
        zend_helpers::object_property_bool(deliver_frame, "redelivered").unwrap_or(false);
    let route = if !queue.is_empty() {
        queue.clone()
    } else if !routing_key.is_empty() {
        routing_key.clone()
    } else if !exchange.is_empty() {
        exchange.clone()
    } else {
        "amqp".to_owned()
    };

    // Wire context, from the header frame's application table. String values
    // only, trimmed; anything else is absent — `ContentHeaderFrame::$headers`
    // legitimately carries ints and nested tables.
    let header_frame = zend_helpers::object_property_object(channel, "headerFrame");
    let application_header = |key: &str| -> String {
        header_frame
            .and_then(|frame| {
                zend_helpers::object_property_table_string(frame, "headers", key, 4096)
            })
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    let traceparent = application_header("traceparent");
    let tracestate = application_header("tracestate");
    let baggage = application_header("baggage");
    let enqueued_at = application_header(messaging::ENQUEUED_AT_HEADER);
    let schema_header = application_header(messaging::SCHEMA_HEADER);
    // The FIRST wall reading, before any of the work below, so the queue wait
    // is not inflated by the cost of measuring it.
    let started_at_seconds = epoch_seconds_now();
    // The payload, read at BEGIN — `onBodyComplete`'s own body consumes the
    // buffer before it invokes the callback, so this is the last moment the
    // bytes exist. `Buffer::$buffer` is private; unreadable ⇒ no payload, never
    // a failure.
    let body = zend_helpers::object_property_object(channel, "bodyBuffer")
        .and_then(|buffer| zend_helpers::object_property_bytes(buffer, "buffer"));

    // Open the message-scoped request. 'QUEUE' routes it onto the
    // background-job profile rates; the empty service name falls back to the
    // envelope's application id so consumer and web land on one map node; the
    // wire `sampled` flag is honoured by `TraceContext::from_header`; a missing
    // traceparent roots a NEW trace — an id is never fabricated.
    crate::start_request(
        &traceparent,
        &tracestate,
        &baggage,
        "",
        "",
        "",
        "QUEUE".to_owned(),
        &route,
        String::new(),
    );
    if !crate::chronos_request_active() {
        // The collector declined (disabled, no envelope). Untraced, unharmed.
        observe_only_fallback(execute_data, name, interned);
        return;
    }

    // The consume facts — `consumeAttributes()`'s exact keys, absent-never-
    // guessed. The vhost comes off `channel->client->options`, the protected
    // member the bespoke bridge had to take as a parameter.
    let client = zend_helpers::object_property_object(channel, "client");
    let read_option = |key: &str| {
        client
            .and_then(|object| {
                zend_helpers::object_property_table_string(object, "options", key, 256)
            })
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    let vhost = read_option("vhost");
    let host = read_option("host");
    let port = read_option("port");
    let header_property = |property: &str| -> String {
        header_frame
            .and_then(|frame| zend_helpers::object_property_string(frame, property, 512))
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    let content_type = header_property("contentType");

    let destination = messaging::amqp_destination(&vhost, &exchange, &routing_key, &queue);
    let mut facts: Vec<(String, String)> = vec![
        ("span.kind".into(), "consumer".into()),
        ("messaging.system".into(), messaging::SYSTEM_RABBITMQ.into()),
        // Process, not receive: only the handler has a duration worth looking at.
        ("messaging.operation".into(), "process".into()),
    ];
    for (key, value) in destination.attributes() {
        facts.push((key.into(), value));
    }
    if !host.is_empty() {
        facts.push(("server.address".into(), host));
    }
    if !port.is_empty() {
        facts.push(("server.port".into(), port));
    }
    // Null when the stamp is missing, unparseable or in the future — never
    // zero, which would let an unmeasured queue report the healthiest wait.
    if let Some(waited) = messaging::wait_milliseconds(&enqueued_at, started_at_seconds) {
        facts.push((
            "messaging.message.queue_time_ms".into(),
            waited.to_string(),
        ));
    }
    // AMQP's only retry signal — what separates "slow" from "failing and being
    // redelivered", which look identical without it.
    if redelivered {
        facts.push(("messaging.message.redelivered".into(), "true".into()));
    }
    let message_id = header_property("messageId");
    if !message_id.is_empty() {
        facts.push(("messaging.message.id".into(), message_id));
    }
    let correlation_id = header_property("correlationId");
    if !correlation_id.is_empty() {
        facts.push(("messaging.message.conversation_id".into(), correlation_id));
    }
    // messaging.message.name: THE GAP's fix. Precedence (header over property,
    // and why) is `messaging::resolve_message_name`'s docblock.
    let type_header = header_property("typeHeader");
    if let Some(message_name) = messaging::resolve_message_name(&schema_header, &type_header) {
        facts.push(("messaging.message.name".into(), message_name));
    }
    if let Some(word) = messaging::protocol(&content_type) {
        facts.push(("messaging.protocol".into(), word.into()));
    }
    if !content_type.is_empty() {
        facts.push((
            "messaging.message.body.content_type".into(),
            content_type.clone(),
        ));
    }
    if let Some(bytes) = &body {
        facts.push(("messaging.message.body.size".into(), bytes.len().to_string()));
        if messaging::capture_enabled() {
            // 8192 — the request-attribute bag's own MAX_VALUE_BYTES: the
            // truncation happens where `.truncated` can be set honestly, not in
            // the bag's cap where an oversized body would arrive looking
            // complete.
            let preview_cap = messaging::CONSUME_PREVIEW_CAP.min(messaging::body_ceiling());
            if let Some(encoded) = messaging::encode_body(bytes, preview_cap) {
                facts.push(("messaging.message.body".into(), encoded.body));
                if encoded.base64 {
                    facts.push(("messaging.message.body.encoding".into(), "base64".into()));
                }
                if encoded.truncated {
                    facts.push(("messaging.message.body.truncated".into(), "true".into()));
                }
            }
            // Whole copy keyed by the REQUEST ROOT (empty ids), because the
            // preview rides the request-attribute bag and lands on the root —
            // the span that promises the payload is the span that owns it.
            if let Some((payload, encoding)) = messaging::whole_body(
                bytes,
                messaging::CONSUME_PREVIEW_CAP,
                messaging::body_ceiling(),
            ) {
                if crate::store_message_body(
                    String::new(),
                    String::new(),
                    content_type.clone(),
                    payload,
                    encoding.to_owned(),
                ) {
                    facts.push(("messaging.message.body.stored".into(), "true".into()));
                }
            }
        }
    }
    // Deliberately omitted, as the bridge always did: delivery tag and consumer
    // tag (per-connection, unbounded, useless to join on) and any consumer-group
    // word (AMQP has none; borrowing Kafka's would invent a concept).

    crate::request_attributes::merge(facts.iter().cloned());
    // The in-flight marker, AFTER the request is open — it names the span that
    // will close it, and that span does not exist until now. Timeout 0 (`None`
    // downstream): AMQP gives a consumer no per-message deadline, and a guessed
    // one reaps live work.
    crate::chronos_job_started(route.clone(), 0, facts.into_iter().collect());

    // `LAST_THROW` was cleared by `set_request_context`, so the request IS the
    // throw window — no separate sequence counter needed.
    DELIVERY_SCOPE.with(|slot| *slot.borrow_mut() = Some(DeliveryRoute::Known(route)));
    let mut frame = CallFrame::observe_only(name.clone());
    frame.owns_delivery_scope = true;
    let arguments = capture_tier_three_arguments(execute_data, interned);
    record_call_path_enter(name, interned.internal, interned.file(), &arguments);
    push_frame(frame, interned.id);
}

/// CONSUME scoping for `PhpAmqpLib\Channel\AMQPChannel::basic_deliver` — the
/// amqplib observation contract's "Consume scoping" section.
///
/// Args: 0=`$reader` (`AMQPReader`), 1=`$message` (`AMQPMessage`, already
/// fully hydrated: `load_properties` and the body have already run by
/// entry — verified against the vendored source, both happen on a
/// content-header/body sequence read BEFORE this method-frame dispatch).
/// What is NOT yet populated at BEGIN: the delivery envelope (consumer tag,
/// exchange, routing key, redelivered) — those are the FIRST statements of
/// this call's own body, read off `$reader`, and reading them ourselves here
/// would consume the same cursor and corrupt the real parse. See
/// `DeliveryRoute::DeferredAmqplib`'s docblock for the full reasoning and
/// `resolve_amqplib_delivery_route` for where the envelope is actually read,
/// at CLOSE.
///
/// This method is `protected` — irrelevant to `zend_observer`, which hooks by
/// opcode/execute_data regardless of visibility, same as every other
/// non-public method already in these tables.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid execute_data.
#[cfg(feature = "zend-observer")]
unsafe fn begin_messaging_delivery_amqplib(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
) {
    use crate::messaging;

    if crate::chronos_request_active() {
        observe_only_fallback(execute_data, name, interned);
        return;
    }
    let Some(message) = zend_helpers::arg_object(execute_data, 1) else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };

    // Message content: readable NOW (see this function's own docblock) —
    // `content_type`/`type` are plain scalar keys directly in `$properties`
    // (siblings of `application_headers`, not nested under it), the same
    // shape the publish side already reads.
    let content_type =
        zend_helpers::object_property_table_string(message, "properties", "content_type", 256)
            .map(|value| value.trim().to_owned())
            .unwrap_or_default();
    let type_header =
        zend_helpers::object_property_table_string(message, "properties", "type", 512)
            .map(|value| value.trim().to_owned())
            .unwrap_or_default();
    let message_id =
        zend_helpers::object_property_table_string(message, "properties", "message_id", 256)
            .map(|value| value.trim().to_owned())
            .unwrap_or_default();
    let correlation_id =
        zend_helpers::object_property_table_string(message, "properties", "correlation_id", 256)
            .map(|value| value.trim().to_owned())
            .unwrap_or_default();
    // `$msg->body` is a public, non-deprecated-for-reading string property,
    // read as RAW bytes — the text-or-base64 decision must see the payload
    // as published, not a lossy UTF-8 conversion of it.
    let body = zend_helpers::object_property_bytes(message, "body");

    // Wire context: the three-shape header carrier's READ side
    // (`amqp_application_header`), identical mechanism to the publish side's
    // write. The FIRST wall reading, before any further work, so the queue
    // wait is not inflated by the cost of measuring it.
    let traceparent =
        zend_helpers::amqp_application_header(message, "traceparent", 256).unwrap_or_default();
    let tracestate =
        zend_helpers::amqp_application_header(message, "tracestate", 4096).unwrap_or_default();
    let baggage =
        zend_helpers::amqp_application_header(message, "baggage", 4096).unwrap_or_default();
    let enqueued_at =
        zend_helpers::amqp_application_header(message, messaging::ENQUEUED_AT_HEADER, 64)
            .unwrap_or_default();
    let schema_header =
        zend_helpers::amqp_application_header(message, messaging::SCHEMA_HEADER, 512)
            .unwrap_or_default();
    let started_at_seconds = epoch_seconds_now();

    // Open the message-scoped request. The route passed here is
    // provisional — see `DeliveryRoute::DeferredAmqplib`'s docblock — it
    // only ever reaches the profiler's "route" label on a request that is
    // ALSO profiled, never the recorded span (that comes from
    // `close_messaging_delivery`'s own, fully-resolved, `route_pattern`).
    crate::start_request(
        &traceparent,
        &tracestate,
        &baggage,
        "",
        "",
        "",
        "QUEUE".to_owned(),
        "amqp",
        String::new(),
    );
    if !crate::chronos_request_active() {
        observe_only_fallback(execute_data, name, interned);
        return;
    }

    // vhost/host/port: the three-hop protected chain off `$this` (the
    // channel), same as the publish side — the contract's own "No gap" note:
    // the connection is by definition already live at consume time (frames
    // are being read off it).
    let channel = zend_helpers::this_object(execute_data);
    let connection = channel.and_then(|c| zend_helpers::object_property_object(c, "connection"));
    let io = connection.and_then(|c| zend_helpers::object_property_object(c, "io"));
    let host = io
        .and_then(|io| zend_helpers::object_property_string(io, "host", 256))
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let port = io
        .and_then(|io| zend_helpers::object_property_string(io, "port", 32))
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();

    let mut facts: Vec<(String, String)> = vec![
        ("span.kind".into(), "consumer".into()),
        ("messaging.system".into(), messaging::SYSTEM_RABBITMQ.into()),
        ("messaging.operation".into(), "process".into()),
    ];
    if !host.is_empty() {
        facts.push(("server.address".into(), host));
    }
    if !port.is_empty() {
        facts.push(("server.port".into(), port));
    }
    if let Some(waited) = messaging::wait_milliseconds(&enqueued_at, started_at_seconds) {
        facts.push(("messaging.message.queue_time_ms".into(), waited.to_string()));
    }
    if !message_id.is_empty() {
        facts.push(("messaging.message.id".into(), message_id));
    }
    if !correlation_id.is_empty() {
        facts.push(("messaging.message.conversation_id".into(), correlation_id));
    }
    if let Some(name) = messaging::resolve_message_name(&schema_header, &type_header) {
        facts.push(("messaging.message.name".into(), name));
    }
    if let Some(word) = messaging::protocol(&content_type) {
        facts.push(("messaging.protocol".into(), word.into()));
    }
    if !content_type.is_empty() {
        facts.push((
            "messaging.message.body.content_type".into(),
            content_type.clone(),
        ));
    }
    if let Some(bytes) = &body {
        facts.push(("messaging.message.body.size".into(), bytes.len().to_string()));
        if messaging::capture_enabled() {
            let preview_cap = messaging::CONSUME_PREVIEW_CAP.min(messaging::body_ceiling());
            if let Some(encoded) = messaging::encode_body(bytes, preview_cap) {
                facts.push(("messaging.message.body".into(), encoded.body));
                if encoded.base64 {
                    facts.push(("messaging.message.body.encoding".into(), "base64".into()));
                }
                if encoded.truncated {
                    facts.push(("messaging.message.body.truncated".into(), "true".into()));
                }
            }
            if let Some((payload, encoding)) = messaging::whole_body(
                bytes,
                messaging::CONSUME_PREVIEW_CAP,
                messaging::body_ceiling(),
            ) {
                if crate::store_message_body(
                    String::new(),
                    String::new(),
                    content_type.clone(),
                    payload,
                    encoding.to_owned(),
                ) {
                    facts.push(("messaging.message.body.stored".into(), "true".into()));
                }
            }
        }
    }

    crate::request_attributes::merge(facts.iter().cloned());
    // Provisional name — see `DeliveryRoute::DeferredAmqplib`'s docblock: the
    // real destination is not readable until `resolve_amqplib_delivery_route`
    // runs at close, and this in-flight marker cannot wait for that.
    crate::chronos_job_started("amqp".to_owned(), 0, facts.into_iter().collect());

    DELIVERY_SCOPE.with(|slot| *slot.borrow_mut() = Some(DeliveryRoute::DeferredAmqplib));
    let mut frame = CallFrame::observe_only(name.clone());
    frame.owns_delivery_scope = true;
    let arguments = capture_tier_three_arguments(execute_data, interned);
    record_call_path_enter(name, interned.internal, interned.file(), &arguments);
    push_frame(frame, interned.id);
}

/// CONSUME scoping for `Basis\Nats\Client::processMsg` — the NATS contract
/// §5(a)/(b)/(c): a message-scoped request when `$handler` is a real
/// callable dispatching an inbound job, `ObserveOnly` (no request) when it
/// is a `Queue` buffer (§5(b): no application code runs inside this call at
/// all) or a reply to this client's OWN `dispatch()`/`request()` round trip
/// (§5(c): that round trip already has its own client-shaped span, see
/// `MESSAGING_NATS_RPC_METHOD`'s docblock — attributing the reply as a
/// "consume" would double the wait-time telemetry against a call that is
/// not a queue consumer at all).
///
/// Args: 0=`$handler` (callable|Queue), 1=`$message` (`Msg`), 2=`$reply`
/// (bool, unused here). `$message` is fully parsed by the time `processMsg`
/// is entered (the wire frame was read earlier, in `Connection::getMessage`)
/// — unlike amqplib's `basic_deliver`, EVERYTHING this handler needs is
/// already a plain property at BEGIN, so (unlike `DeliveryRoute::DeferredAmqplib`)
/// this scope's route is always `Known`.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid execute_data.
#[cfg(feature = "zend-observer")]
unsafe fn begin_messaging_delivery_nats(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
) {
    use crate::messaging;

    if crate::chronos_request_active() {
        observe_only_fallback(execute_data, name, interned);
        return;
    }
    // §5(b): a bare `Queue` handler only buffers the message for a later
    // `Queue::fetchAll` poll — no application code runs here at all.
    if zend_helpers::arg_is_instance_of(execute_data, 0, "Basis\\Nats\\Queue") {
        observe_only_fallback(execute_data, name, interned);
        return;
    }
    let Some(message) = zend_helpers::arg_object(execute_data, 1) else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    let subject = zend_helpers::object_property_string(message, "subject", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    // §5(c): a reply to this client's own round trip is not an inbound job —
    // `requestsSubject` is private but engine-readable off `$this` (the
    // `Client`) regardless, same as every other privacy-irrelevant read in
    // this file.
    if let Some(client) = zend_helpers::this_object(execute_data) {
        let requests_subject =
            zend_helpers::object_property_string(client, "requestsSubject", 64)
                .unwrap_or_default();
        if !requests_subject.is_empty() && subject.starts_with(&requests_subject) {
            observe_only_fallback(execute_data, name, interned);
            return;
        }
    }

    // Naming: the SUBSCRIBE bank's subject for this sid (§4) — client-minted
    // before the wire write, so always known by dispatch time unless the
    // local map was cleared under a reconnect — falling back to the
    // message's own subject when the sid was never banked, honestly rather
    // than inventing one.
    let sid = zend_helpers::object_property_string(message, "sid", 64).unwrap_or_default();
    let banked = if sid.is_empty() {
        None
    } else {
        SUBSCRIBE_SIDS.with(|map| map.borrow().get(&sid).cloned())
    };
    let (banked_subject, group) = banked.unwrap_or_default();
    let route = if !banked_subject.is_empty() {
        banked_subject
    } else if !subject.is_empty() {
        subject.clone()
    } else {
        messaging::SYSTEM_NATS.to_owned()
    };

    let payload = zend_helpers::object_property_object(message, "payload");
    let header = |key: &str| -> String {
        payload
            .and_then(|object| {
                zend_helpers::object_property_table_string(object, "headers", key, 4096)
            })
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    let traceparent = header("traceparent");
    let tracestate = header("tracestate");
    let baggage = header("baggage");
    let enqueued_at = header(messaging::ENQUEUED_AT_HEADER);
    let schema_header = header(messaging::SCHEMA_HEADER);
    let content_type = header("content-type");
    // The FIRST wall reading, before any further work.
    let started_at_seconds = epoch_seconds_now();
    let body = payload.and_then(|object| zend_helpers::object_property_bytes(object, "body"));

    crate::start_request(
        &traceparent,
        &tracestate,
        &baggage,
        "",
        "",
        "",
        "QUEUE".to_owned(),
        &route,
        String::new(),
    );
    if !crate::chronos_request_active() {
        observe_only_fallback(execute_data, name, interned);
        return;
    }

    // server.address/server.port: a ZERO-hop read off `$this` (the
    // `Client`) — `$this->configuration` is public, unlike Bunny's protected
    // `$options` array, and simpler than amqplib's three-hop chain.
    let client = zend_helpers::this_object(execute_data);
    let configuration =
        client.and_then(|object| zend_helpers::object_property_object(object, "configuration"));
    let host = configuration
        .and_then(|object| zend_helpers::object_property_string(object, "host", 256))
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    // `Configuration::$port` is a declared `int`; `object_property_string`
    // already coerces a LONG property the same way `zval_to_owned_string`
    // always has, so no separate numeric read is needed for a value that
    // only ever rides as a string attribute.
    let port = configuration
        .and_then(|object| zend_helpers::object_property_string(object, "port", 16))
        .unwrap_or_default();

    let destination = messaging::nats_destination(&subject);
    let mut facts: Vec<(String, String)> = vec![
        ("span.kind".into(), "consumer".into()),
        ("messaging.system".into(), messaging::SYSTEM_NATS.into()),
        ("messaging.operation".into(), "process".into()),
    ];
    for (key, value) in destination.attributes() {
        facts.push((key.into(), value));
    }
    if !host.is_empty() {
        facts.push(("server.address".into(), host));
    }
    if !port.is_empty() {
        facts.push(("server.port".into(), port));
    }
    // Core NATS queue groups ARE the shared-work-queue mechanism — the
    // direct analog of a Kafka consumer group (NATS contract §3).
    if !group.is_empty() {
        facts.push(("messaging.consumer.group.name".into(), group));
    }
    // Core NATS has no redelivery concept at all (contract §7) — absent,
    // never guessed, unlike AMQP's real `redelivered` flag.
    if let Some(waited) = messaging::wait_milliseconds(&enqueued_at, started_at_seconds) {
        facts.push(("messaging.message.queue_time_ms".into(), waited.to_string()));
    }
    // GAP 1 on NATS: no `type`-property equivalent exists at all, so the
    // header is the whole story — `resolve_message_name` is called with an
    // always-empty second argument, exactly as the RdKafka path does.
    if let Some(message_name) = messaging::resolve_message_name(&schema_header, "") {
        facts.push(("messaging.message.name".into(), message_name));
    }
    if let Some(word) = messaging::protocol(&content_type) {
        facts.push(("messaging.protocol".into(), word.into()));
    }
    if !content_type.is_empty() {
        facts.push((
            "messaging.message.body.content_type".into(),
            content_type.clone(),
        ));
    }
    if let Some(bytes) = &body {
        facts.push(("messaging.message.body.size".into(), bytes.len().to_string()));
        if messaging::capture_enabled() {
            let preview_cap = messaging::CONSUME_PREVIEW_CAP.min(messaging::body_ceiling());
            if let Some(encoded) = messaging::encode_body(bytes, preview_cap) {
                facts.push(("messaging.message.body".into(), encoded.body));
                if encoded.base64 {
                    facts.push(("messaging.message.body.encoding".into(), "base64".into()));
                }
                if encoded.truncated {
                    facts.push(("messaging.message.body.truncated".into(), "true".into()));
                }
            }
            if let Some((payload_bytes, encoding)) = messaging::whole_body(
                bytes,
                messaging::CONSUME_PREVIEW_CAP,
                messaging::body_ceiling(),
            ) {
                if crate::store_message_body(
                    String::new(),
                    String::new(),
                    content_type.clone(),
                    payload_bytes,
                    encoding.to_owned(),
                ) {
                    facts.push(("messaging.message.body.stored".into(), "true".into()));
                }
            }
        }
    }

    crate::request_attributes::merge(facts.iter().cloned());
    crate::chronos_job_started(route.clone(), 0, facts.into_iter().collect());

    DELIVERY_SCOPE.with(|slot| *slot.borrow_mut() = Some(DeliveryRoute::Known(route)));
    let mut frame = CallFrame::observe_only(name.clone());
    frame.owns_delivery_scope = true;
    let arguments = capture_tier_three_arguments(execute_data, interned);
    record_call_path_enter(name, interned.internal, interned.file(), &arguments);
    push_frame(frame, interned.id);
}

/// NATS's single wire choke point (NATS contract §0): `Connection::sendMessage`
/// serves THREE roles decided only by arg 0's RUNTIME class, which is why it
/// has its own `SpanPolicy` (`MessagingNatsSend`) instead of a fixed
/// method-name-keyed verdict:
///
///   * `Basis\Nats\Message\Publish` — PUBLISH span + header injection
///     (`begin_nats_publish`).
///   * `Basis\Nats\Message\Subscribe` — side-channel banking only, no span
///     (`bank_nats_subscribe`, NATS contract §4) — client-minted `sid`,
///     banked BEFORE the wire write, so (unlike Bunny/amqplib's
///     server-assigned consumer tags) no END-time hook is needed at all.
///   * anything else (ping/pong/connect/unsubscribe/…) — a plain
///     pass-through frame, identical to an unlisted userland call.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid execute_data.
#[cfg(feature = "zend-observer")]
unsafe fn begin_nats_send_message(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
) {
    let Some(message) = zend_helpers::arg_object(execute_data, 0) else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    // Runtime dispatch by REAL `instanceof` (parent-chain walk, the
    // contract's own wording), not an exact class-name compare: a userland
    // subclass of `Publish`/`Subscribe` is still that message on the wire —
    // `Publish` and `Subscribe` both extend `Prototype` and never each
    // other, so the two tests stay mutually exclusive.
    if zend_helpers::object_instance_of(message, "Basis\\Nats\\Message\\Publish") {
        // The per-request suppression seam gates this branch exactly as it
        // gates `MessagingPublish` in the begin trampoline (a userland NATS
        // bridge that declared ownership gets no duplicate native span and
        // no header write) — checked HERE rather than at the policy switch
        // because `MessagingNatsSend` is not only a publish: the Subscribe
        // banking below must keep running under suppression (it is a
        // process-lifetime side channel, like Bunny's `CONSUME_FNS`
        // tracking, which the seam never gates either — suppressing it for
        // one request would blind every later delivery's queue naming).
        if SUPPRESS_MESSAGING.with(std::cell::Cell::get) {
            observe_only_fallback(execute_data, name, interned);
            return;
        }
        begin_nats_publish(execute_data, name, interned, message)
    } else if zend_helpers::object_instance_of(message, "Basis\\Nats\\Message\\Subscribe") {
        bank_nats_subscribe(message);
        observe_only_fallback(execute_data, name, interned);
    } else {
        observe_only_fallback(execute_data, name, interned);
    }
}

/// PUBLISH observation for NATS's `Publish` message — the NATS observation
/// contract §2/§3 in full.
///
/// `message` is arg 0 of `Connection::sendMessage`, already confirmed
/// `instanceof Publish` by the caller (`begin_nats_send_message`).
/// `$message->payload` is unconditionally a `Payload` object by the time
/// `sendMessage` sees it — `Payload::parse()` has already run inside
/// `Client::publish()`'s body before this call (contract §1) — so treating
/// its absence as "decline, don't crash" is defensive, not an expected path.
///
/// Injection target: `$message->payload->headers`, via
/// `inject_property_array_entries` — the two-hop object-property array
/// write (contract §2/§8.2), add-if-absent, same semantics as
/// `inject_array_entries`'s argument-slot write.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid execute_data and
/// a live `message` object (arg 0, already type-checked by the caller).
#[cfg(feature = "zend-observer")]
unsafe fn begin_nats_publish(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    name: &std::rc::Rc<str>,
    interned: &crate::deterministic::InternedFunction,
    message: *mut ext_php_rs::ffi::zend_object,
) {
    use crate::messaging;

    let subject = zend_helpers::object_property_string(message, "subject", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let Some(payload) = zend_helpers::object_property_object(message, "payload") else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    // RAW bytes: the text-or-base64 decision must see the payload as
    // published, not a lossy UTF-8 conversion of it.
    let body = zend_helpers::object_property_bytes(payload, "body");
    let content_type =
        zend_helpers::object_property_table_string(payload, "headers", "content-type", 256)
            .map(|value| value.trim().to_owned())
            .unwrap_or_default();
    // GAP 1 on NATS: no AMQP-`type`-property equivalent exists at all, so
    // the `serializeToString` bank is the WHOLE story here, exactly as for
    // RdKafka — `resolve_message_name` is not even called; the bank lookup
    // IS the resolution.
    let message_name = body
        .as_deref()
        .and_then(|bytes| MESSAGE_BANK.with(|bank| bank.borrow().lookup(bytes).map(str::to_owned)))
        .unwrap_or_default();

    let context = REQUEST_CONTEXT.with(|ctx| ctx.borrow().as_ref().cloned());
    let frame = context
        .as_ref()
        .map(|ctx| on_begin(ctx, name, SpanPolicy::MessagingPublish));

    let mut entries: Vec<(&str, String)> = Vec::new();
    if let Some(frame) = &frame {
        if let Some(traceparent) = &frame.injected_traceparent {
            entries.push(("traceparent", traceparent.clone()));
        }
    }
    if let Some(ctx) = &context {
        if let Some(tracestate) = &ctx.tracestate {
            entries.push(("tracestate", tracestate.clone()));
        }
        if let Some(baggage) = &ctx.baggage {
            entries.push(("baggage", baggage.clone()));
        }
    }
    entries.push((
        messaging::ENQUEUED_AT_HEADER,
        messaging::enqueued_at_stamp(epoch_seconds_now()),
    ));
    if !message_name.is_empty() {
        entries.push((messaging::SCHEMA_HEADER, message_name.clone()));
    }
    let injection = zend_helpers::inject_property_array_entries(payload, "headers", &entries);
    let propagated = injection.is_ok();
    if let Err(reason) = injection {
        warn_injection_failure_for(MESSAGING_NATS_SEND_METHOD, reason);
    }

    let Some(mut frame) = frame else {
        observe_only_fallback(execute_data, name, interned);
        return;
    };
    frame.bypass_span_cap = propagated;

    let destination = messaging::nats_destination(&subject);
    frame.span_name = Some(messaging::labeled("PUBLISH", &destination, messaging::SYSTEM_NATS));

    frame.attributes.push(("span.kind".into(), "producer".into()));
    frame
        .attributes
        .push(("messaging.system".into(), messaging::SYSTEM_NATS.into()));
    frame
        .attributes
        .push(("messaging.operation".into(), "publish".into()));
    for (key, value) in destination.attributes() {
        frame.attributes.push((key.into(), value));
    }
    if let Some(word) = messaging::protocol(&content_type) {
        frame
            .attributes
            .push(("messaging.protocol".into(), word.into()));
    }
    if !content_type.is_empty() {
        frame.attributes.push((
            "messaging.message.body.content_type".into(),
            content_type.clone(),
        ));
    }
    if !message_name.is_empty() {
        frame
            .attributes
            .push(("messaging.message.name".into(), message_name));
    }
    if let Some(bytes) = &body {
        frame
            .attributes
            .push(("messaging.message.body.size".into(), bytes.len().to_string()));
        if messaging::capture_enabled() {
            let preview_cap = messaging::PUBLISH_PREVIEW_CAP.min(messaging::body_ceiling());
            if let Some(encoded) = messaging::encode_body(bytes, preview_cap) {
                frame
                    .attributes
                    .push(("messaging.message.body".into(), encoded.body));
                if encoded.base64 {
                    frame
                        .attributes
                        .push(("messaging.message.body.encoding".into(), "base64".into()));
                }
                if encoded.truncated {
                    frame
                        .attributes
                        .push(("messaging.message.body.truncated".into(), "true".into()));
                }
            }
            if let Some((payload_bytes, encoding)) = messaging::whole_body(
                bytes,
                messaging::PUBLISH_PREVIEW_CAP,
                messaging::body_ceiling(),
            ) {
                if crate::store_message_body(
                    frame.trace_id.clone(),
                    frame.span_id.clone(),
                    content_type.clone(),
                    payload_bytes,
                    encoding.to_owned(),
                ) {
                    frame
                        .attributes
                        .push(("messaging.message.body.stored".into(), "true".into()));
                }
            }
        }
    }
    if let Some(file) = interned.file() {
        frame
            .attributes
            .push(("code.filepath".into(), file.to_owned()));
    }

    let arguments = capture_tier_three_arguments(execute_data, interned);
    record_call_path_enter(name, interned.internal, interned.file(), &arguments);
    push_frame(frame, interned.id);
}

/// Bank one `Subscribe` message's `sid → (subject, group)` — NATS contract
/// §4. Client-minted `sid`, already set BEFORE this call (constructed before
/// `sendMessage` runs, same "always positional, no defaults" guarantee as
/// the `Publish` branch), so no END-time hook is needed the way Bunny/amqplib
/// need one for their server-assigned consumer tags.
///
/// # Safety
/// Called from the Zend observer begin handler with a valid, live `message`
/// object (arg 0 of `Connection::sendMessage`, already confirmed
/// `instanceof Subscribe`).
#[cfg(feature = "zend-observer")]
unsafe fn bank_nats_subscribe(message: *mut ext_php_rs::ffi::zend_object) {
    let Some(sid) = zend_helpers::object_property_string(message, "sid", 64) else {
        return;
    };
    if sid.is_empty() {
        return;
    }
    let subject = zend_helpers::object_property_string(message, "subject", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let group = zend_helpers::object_property_string(message, "group", 256)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    SUBSCRIBE_SIDS.with(|map| {
        let mut map = map.borrow_mut();
        if map.len() >= MAX_SUBSCRIBE_SIDS && !map.contains_key(&sid) {
            return;
        }
        map.insert(sid, (subject, group));
    });
}

/// `SpanPolicy::MessagingPoll`'s END-time reader, dispatching to whichever
/// broker's retval shape `frame.name` needs — see that policy's own
/// docblock for why almost everything worth saying about a pull-style
/// consume call is only knowable here, never at BEGIN.
///
/// # Safety
/// Called from the Zend observer end handler with a valid `execute_data` and
/// `retval` for the call being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn capture_messaging_poll_result(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
    frame: &mut CallFrame,
) {
    match frame.name.as_ref() {
        "RdKafka\\KafkaConsumer::consume" | "RdKafka\\ConsumerTopic::consume" => {
            capture_rdkafka_poll_result(retval, frame);
        }
        "Basis\\Nats\\Queue::fetchAll" => {
            capture_nats_fetchall_result(execute_data, retval, frame);
        }
        _ => {}
    }
}

/// RdKafka's poll-span reader (RdKafka contract, "the poll-span contract"):
/// `retval` is the `Message` `KafkaConsumer::consume`/`ConsumerTopic::consume`
/// returned (non-null for the former, nullable for the latter — a null
/// retval reads as an honestly empty poll, not a failure: `Message::err`
/// being `RD_KAFKA_RESP_ERR__PARTITION_EOF`/`__TIMED_OUT` is normal and never
/// sets span status, which comes from `threw` alone regardless of policy).
///
/// # Safety
/// Called from the Zend observer end handler with a valid `retval` for the
/// call being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn capture_rdkafka_poll_result(retval: *mut ext_php_rs::ffi::zval, frame: &mut CallFrame) {
    use crate::messaging;

    frame.attributes.push(("span.kind".into(), "consumer".into()));
    frame
        .attributes
        .push(("messaging.system".into(), messaging::SYSTEM_KAFKA.into()));
    frame
        .attributes
        .push(("messaging.operation".into(), "receive".into()));

    let Some(message) = zend_helpers::retval_object(retval) else {
        frame.span_name = Some(format!("RECEIVE {}", messaging::SYSTEM_KAFKA));
        return;
    };

    let topic = zend_helpers::object_property_string(message, "topic_name", 512)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let destination = messaging::kafka_destination(&topic);
    frame.span_name = Some(messaging::labeled("RECEIVE", &destination, messaging::SYSTEM_KAFKA));
    for (key, value) in destination.attributes() {
        frame.attributes.push((key.into(), value));
    }

    // Partition is explicitly not identity (see `kafka_partition_attribute`'s
    // docblock) — informational only, never fed into `Destination`.
    if let Some(partition) = zend_helpers::object_property_long(message, "partition")
        .and_then(messaging::kafka_partition_attribute)
    {
        frame
            .attributes
            .push(("messaging.kafka.destination.partition".into(), partition));
    }
    if let Some(key) = zend_helpers::object_property_string(message, "key", 256)
        .filter(|value| !value.is_empty())
    {
        frame.attributes.push(("messaging.kafka.message.key".into(), key));
    }
    // Offset is knowable ONLY on the consume side (unlike partition, which a
    // publisher can also name) — this is the one place it is ever recorded.
    if let Some(offset) = zend_helpers::object_property_long(message, "offset") {
        frame
            .attributes
            .push(("messaging.kafka.message.offset".into(), offset.to_string()));
    }

    // Header context: `Message::headers` is a plain string-keyed array,
    // empty (never null) whenever `err != RD_KAFKA_RESP_ERR_NO_ERROR` — the
    // table reads simply come back absent for an empty poll, no special
    // casing needed.
    let header = |key: &str| -> Option<String> {
        zend_helpers::object_property_table_string(message, "headers", key, 4096)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    if let Some(schema) = header(messaging::SCHEMA_HEADER) {
        // GAP 1 on the consume side: Kafka has no `type` property at all, so
        // this header is the ONLY source — `resolve_message_name` is not
        // even called; an empty schema header means an absent name, exactly
        // like the publish side's bank-only resolution.
        frame
            .attributes
            .push(("messaging.message.name".into(), schema));
    }
    if let Some(enqueued_at) = header(messaging::ENQUEUED_AT_HEADER) {
        if let Some(waited) = messaging::wait_milliseconds(&enqueued_at, epoch_seconds_now()) {
            frame
                .attributes
                .push(("messaging.message.queue_time_ms".into(), waited.to_string()));
        }
    }
    // Non-causal: captured for debugging only, never used to parent this
    // span — the RdKafka contract's own "no causality link" section. This
    // span's parent was already fixed at BEGIN, before this header was even
    // knowable.
    if let Some(traceparent) = header("traceparent") {
        frame
            .attributes
            .push(("messaging.message.traceparent".into(), traceparent));
    }
}

/// NATS JetStream's poll-span reader (NATS contract §5(d)): `$this` is the
/// `Queue`, `retval` is the batch `fetchAll` returned. No per-message facts
/// attach anywhere — a batch can span several subjects, so there is no
/// single message this span is "about" — only the whole-batch destination
/// (stream/consumer, parsed from the pull-consumer's own request subject)
/// and its size.
///
/// # Safety
/// Called from the Zend observer end handler with a valid `execute_data` and
/// `retval` for the call being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn capture_nats_fetchall_result(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
    frame: &mut CallFrame,
) {
    use crate::messaging;

    frame.attributes.push(("span.kind".into(), "consumer".into()));
    frame
        .attributes
        .push(("messaging.system".into(), messaging::SYSTEM_NATS.into()));
    frame
        .attributes
        .push(("messaging.operation".into(), "receive".into()));

    // `Queue::$launcher` (private `?Publish`) carries the pull-consumer's own
    // request subject, `$JS.API.CONSUMER.MSG.NEXT.<stream>.<consumer>` —
    // parsed EXACTLY (NATS subject-token grammar forbids embedded `.`), never
    // a heuristic. Absent when the queue was never given a launcher (a core
    // NATS subscribe wrapped in a `Queue`, not a JetStream pull consumer) —
    // the destination then reads as `None` everywhere, honestly.
    let queue = zend_helpers::this_object(execute_data);
    let launcher = queue.and_then(|object| zend_helpers::object_property_object(object, "launcher"));
    let launcher_subject =
        launcher.and_then(|object| zend_helpers::object_property_string(object, "subject", 512));
    let destination = launcher_subject
        .as_deref()
        .and_then(messaging::jetstream_next_subject_stream_and_consumer)
        .map(|(stream, consumer)| messaging::nats_jetstream_poll_destination(&stream, &consumer))
        .unwrap_or(messaging::Destination {
            name: None,
            namespace: None,
            via: None,
            route: None,
        });
    frame.span_name = Some(messaging::labeled("RECEIVE", &destination, messaging::SYSTEM_NATS));
    for (key, value) in destination.attributes() {
        frame.attributes.push((key.into(), value));
    }

    // Batch size: the call's own retval length, never guessed — same "only
    // the return value knows" reasoning `bank_prepared_statement` relies on.
    if let Some(count) = zend_helpers::retval_array_len(retval) {
        frame
            .attributes
            .push(("messaging.batch.message_count".into(), count.to_string()));
    }
}

/// Close the message-scoped request the popped frame opened.
///
/// The error rule, and its stated trade: the bespoke `consumeFailed()` carried
/// the APPLICATION's judgement ("this delivery failed"); the native rule is
/// "any exception thrown during the delivery, even caught". In a thin
/// dispatch-decode-handle consumer those are the same set; a consumer whose
/// framework internals throw-and-catch routinely will show false-positive
/// errored deliveries, and the remedy is the process switch
/// (CHRONOS_PHP_MESSAGING_AUTO=0) — not a silent heuristic.
///
///   * `threw` (EG(exception) still set) — the handler's throw is ESCAPING into
///     the event loop: closed as error with `handled=false`, exception untouched.
///   * a LastThrow within the scope, no pending exception — thrown and
///     swallowed: closed as error with `handled=true`. This is the
///     `consumeFailed()` replacement.
///   * neither — a clean delivery.
///
/// HTTP status is always 0: a message has no status code, and borrowing 200
/// would put a number in a column that means something it does not mean.
///
/// # Safety
/// Called from the Zend observer end handler with the SAME valid
/// `execute_data` `chronos_end_trampoline` was itself called with — needed
/// only for a `DeliveryRoute::DeferredAmqplib` scope (see its docblock);
/// `Known` routes ignore it entirely.
#[cfg(feature = "zend-observer")]
unsafe fn close_messaging_delivery(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    threw: bool,
) {
    let route = match DELIVERY_SCOPE.with(|slot| slot.borrow_mut().take()) {
        Some(route) => route.resolve(execute_data),
        None => "amqp".to_owned(),
    };
    let throw_site = |throw: &LastThrow| {
        if throw.file.is_empty() {
            String::new()
        } else {
            format!("{}:{}", throw.file, throw.line)
        }
    };
    let error = match (last_throw(), threw) {
        (Some(throw), escaping) => {
            let site = throw_site(&throw);
            Some((throw.class, throw.message, site, !escaping))
        }
        // Escaping exception the hook never saw (should be unreachable — the
        // hook chains every throw): still an errored delivery, with the honest
        // minimum said about it.
        (None, true) => Some(("Throwable".to_owned(), String::new(), String::new(), false)),
        (None, false) => None,
    };
    crate::end_request(0, route, error);
}

/// Ceiling on remembered prepared statements per request. CURL_HEADERS gets away
/// without one because an application holds a handful of curl handles; an ORM in a
/// loop can prepare per row, and each entry here retains up to 4 KiB of SQL text.
/// Past the cap new prepares fall back to the graceful text-less path above.
#[cfg(feature = "zend-observer")]
const MAX_PREPARED_STATEMENTS: usize = 512;

/// Remember a prepare call's SQL, keyed by the statement object it returned —
/// see PREPARED_STATEMENTS for the map's contract and the miss behaviour.
///
/// The cap refuses NEW handles only: re-preparing into an already-known handle id
/// always lands, because the engine recycles object handles and a recycled id must
/// never keep the PREVIOUS statement's text — a wrong query on a span is worse
/// than no query.
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are
/// both still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn bank_prepared_statement(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
    sql_arg_index: usize,
) {
    let Some(handle) = zend_helpers::retval_object_handle(retval) else {
        return;
    };
    // Same 4096-byte cap as the direct-query capture in `capture_io_detail`, so a
    // prepared query is bounded exactly like an immediate one.
    let Some(statement) = zend_helpers::arg_scalar_string(execute_data, sql_arg_index, 4096)
    else {
        // A brand-new statement object whose SQL could not be read must not
        // inherit whatever a freed statement left banked under this handle id.
        PREPARED_STATEMENTS.with(|map| {
            map.borrow_mut().remove(&handle);
        });
        return;
    };
    PREPARED_STATEMENTS.with(|map| {
        let mut map = map.borrow_mut();
        if map.len() >= MAX_PREPARED_STATEMENTS && !map.contains_key(&handle) {
            return;
        }
        map.insert(handle, statement);
    });
}

/// The `$this`-keyed counterpart of `bank_prepared_statement`, for the prepare
/// calls that mutate an existing statement object (`mysqli_stmt::prepare`,
/// `mysqli_stmt::__construct`) instead of returning a new one.
///
/// Eviction is the point, not an edge case: these are exactly the creation paths
/// the retval-keyed banking never sees, so a recycled object handle could
/// otherwise keep a freed statement's SQL and stamp the wrong query onto this
/// statement's `::execute` span. A failed `prepare()` (returns `false`), a
/// constructor called without a query, or an unreadable argument all EVICT the
/// handle; only a successful prepare with readable SQL re-banks it. The cap
/// refuses NEW handles only, same rule as above — an evicted-or-replaced known
/// handle always lands.
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are
/// both still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn rebank_prepared_statement_for_this(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
    name: &str,
) {
    let Some(handle) = zend_helpers::this_object_handle(execute_data) else {
        return;
    };
    // `mysqli_stmt::prepare` answers a bool; banking on `false` would record SQL
    // the server never compiled. A constructor has no meaningful return value —
    // reaching the end handler at all means it did not throw.
    let succeeded =
        name != "mysqli_stmt::prepare" || zend_helpers::retval_is_true(retval);
    let statement = if succeeded {
        zend_helpers::arg_scalar_string(execute_data, prepare_sql_arg_index(name), 4096)
    } else {
        None
    };
    PREPARED_STATEMENTS.with(|map| {
        let mut map = map.borrow_mut();
        match statement {
            Some(sql) => {
                if map.len() >= MAX_PREPARED_STATEMENTS && !map.contains_key(&handle) {
                    return;
                }
                map.insert(handle, sql);
            }
            None => {
                map.remove(&handle);
            }
        }
    });
}

/// Response detail for a finished `curl_exec`: the connection phase timeline, the
/// status and content type, the peer address, the request headers the application
/// set, and the response body.
///
/// This is the client-side twin of the server-side `http_capture` module, and it
/// writes the SAME attribute keys — `http.request.headers`, `http.response.body`,
/// `http.timeline` — so the desktop's Request / Response / Timeline tabs render an
/// outbound call and an inbound request through one code path.
///
/// Redaction and caps are applied by `http_capture` so an `Authorization:` header on
/// an outbound API call is masked exactly like an inbound one.
///
/// # Safety
/// Called from the Zend observer end handler, where execute_data and retval are both
/// still valid for the frame being unwound.
#[cfg(feature = "zend-observer")]
unsafe fn capture_curl_result(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    retval: *mut ext_php_rs::ffi::zval,
    frame: &mut CallFrame,
) {
    let config = crate::http_capture::current_config();
    if !config.enabled {
        return;
    }
    let info = zend_helpers::curl_info(execute_data);

    // Request headers the application configured on this handle (plus the traceparent
    // the observer merged in), as the same JSON object shape the server side emits.
    if let Some(handle) = zend_helpers::arg_object_handle(execute_data, 0) {
        let headers = CURL_HEADERS.with(|map| map.borrow().get(&handle).cloned());
        if let Some(headers) = headers {
            let pairs: Vec<(String, String)> = headers
                .iter()
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
                .collect();
            if let Some(json) = crate::http_capture::encode_header_map(pairs, &config) {
                frame
                    .attributes
                    .push((crate::http_capture::REQUEST_HEADERS.into(), json));
            }
        }
    }

    let content_type = info.get("content_type").cloned().unwrap_or_default();
    if let Some(code) = info.get("http_code").and_then(|c| c.parse::<i64>().ok()) {
        if code > 0 {
            // Legacy + current semconv spelling, same value (see `http_capture.rs`).
            frame
                .attributes
                .push(("http.status_code".into(), code.to_string()));
            frame.attributes.push((
                crate::http_capture::RESPONSE_STATUS_CODE.into(),
                code.to_string(),
            ));
        }
    }
    if !content_type.is_empty() {
        frame
            .attributes
            .push(("http.response.content_type".into(), content_type.clone()));
    }
    if let Some(ip) = info.get("primary_ip").filter(|ip| !ip.is_empty()) {
        frame.attributes.push(("server.address".into(), ip.clone()));
    }
    if let Some(port) = info.get("primary_port").filter(|p| p.as_str() != "0") {
        frame.attributes.push(("server.port".into(), port.clone()));
    }
    if let Some(method) = info.get("effective_method").filter(|m| !m.is_empty()) {
        frame
            .attributes
            .push(("http.method".into(), method.clone()));
        frame
            .attributes
            .push((crate::http_capture::REQUEST_METHOD.into(), method.clone()));
    }

    // The body, but only when CURLOPT_RETURNTRANSFER made curl_exec return one; a
    // handle writing straight to stdout returns `true`, and "true" is not a payload.
    if config.capture_bodies {
        if let Some(body) = zend_helpers::string_retval(retval) {
            let size = info
                .get("size_download")
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|size| *size > 0)
                .unwrap_or(body.len());
            frame.attributes.extend(crate::http_capture::encode_body(
                crate::http_capture::RESPONSE_BODY,
                &body,
                size,
                &content_type,
                &config,
            ));
        }
    }

    if let Some(timeline) = curl_timeline(&info) {
        frame
            .attributes
            .push((crate::http_capture::TIMELINE.into(), timeline));
    }
}

/// curl's cumulative timing marks -> the phase array the Timeline tab draws.
///
/// Every `*_time` curl reports is measured from the START of the transfer and is
/// cumulative, so consecutive marks subtract into the phase between them. A mark of
/// zero means the phase did not happen (no TLS on plain HTTP, no DNS on a warm
/// connection or an IP literal), and a zero-length phase is dropped rather than
/// drawn as a hairline the reader would have to hover to dismiss.
#[cfg(feature = "zend-observer")]
fn curl_timeline(info: &std::collections::HashMap<String, String>) -> Option<String> {
    let seconds = |key: &str| -> Option<f64> {
        info.get(key)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
    };
    let total = seconds("total_time")?;
    let namelookup = seconds("namelookup_time").unwrap_or(0.0);
    let connect = seconds("connect_time").unwrap_or(namelookup);
    let appconnect = seconds("appconnect_time").unwrap_or(0.0);
    let pretransfer = seconds("pretransfer_time").unwrap_or(connect.max(appconnect));
    let starttransfer = seconds("starttransfer_time").unwrap_or(pretransfer);

    // (name, cumulative end mark). TLS collapses into the connect phase when curl
    // reports no appconnect mark, which is exactly what a plain-HTTP call looks like.
    let marks: Vec<(&str, f64)> = vec![
        ("dns", namelookup),
        ("connect", connect),
        (
            "tls",
            if appconnect > 0.0 {
                appconnect
            } else {
                connect
            },
        ),
        ("send", pretransfer),
        ("wait", starttransfer),
        ("download", total),
    ];

    let mut phases: Vec<(String, u128, u128)> = Vec::new();
    let mut cursor = 0.0f64;
    for (name, end) in marks {
        if end <= cursor {
            continue;
        }
        phases.push((name.to_owned(), to_nanos(cursor), to_nanos(end)));
        cursor = end;
    }
    if phases.is_empty() {
        return None;
    }
    Some(crate::http_capture::encode_phases(&phases))
}

#[cfg(feature = "zend-observer")]
fn to_nanos(seconds: f64) -> u128 {
    (seconds.max(0.0) * 1e9) as u128
}

/// CURLOPT_HTTPHEADER — stable across every curl/PHP version.
#[cfg(feature = "zend-observer")]
const CURLOPT_HTTPHEADER: i64 = 10023;

/// Record the header list an application sets on a curl handle:
/// `curl_setopt($ch, CURLOPT_HTTPHEADER, [...])`.
#[cfg(feature = "zend-observer")]
unsafe fn track_curl_setopt(execute_data: *mut ext_php_rs::ffi::zend_execute_data) {
    let Some(handle) = zend_helpers::arg_object_handle(execute_data, 0) else {
        return;
    };
    let Some(option) = zend_helpers::arg_long(execute_data, 1) else {
        return;
    };
    if option != CURLOPT_HTTPHEADER {
        return;
    }
    let headers = zend_helpers::arg_string_array(execute_data, 2).unwrap_or_default();
    CURL_HEADERS.with(|map| {
        map.borrow_mut().insert(handle, headers);
    });
}

/// Record headers set through `curl_setopt_array($ch, [CURLOPT_HTTPHEADER => [...]])`.
#[cfg(feature = "zend-observer")]
unsafe fn track_curl_setopt_array(execute_data: *mut ext_php_rs::ffi::zend_execute_data) {
    let Some(handle) = zend_helpers::arg_object_handle(execute_data, 0) else {
        return;
    };
    let Some(headers) =
        zend_helpers::arg_array_key_string_array(execute_data, 1, CURLOPT_HTTPHEADER)
    else {
        return;
    };
    CURL_HEADERS.with(|map| {
        map.borrow_mut().insert(handle, headers);
    });
}

/// Inject a traceparent header into the curl handle passed to `curl_exec($ch)`.
///
/// Merge-safe: the header list is the application's tracked CURLOPT_HTTPHEADER value
/// (if any) plus our `traceparent`, applied via a real `curl_setopt` call. The pending
/// traceparent is also published for the userland seam (`chronos_pending_traceparent`).
///
/// # Safety
/// Called from the Zend observer with a valid execute_data pointer.
#[cfg(feature = "zend-observer")]
unsafe fn inject_curl_traceparent(
    execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    traceparent: &str,
) {
    // Publish for userland retrieval regardless of whether direct injection works.
    PENDING_TRACEPARENT.with(|tp| *tp.borrow_mut() = Some(traceparent.to_owned()));

    // Another tracer sharing this process injects from inside the `curl_exec`
    // handler — after this observer hook — so ours would be overwritten and the
    // callee would join ITS trace instead of the one we recorded. Where Chronos
    // is instrumenting, Chronos owns the wire: stand the other tracer's
    // propagation down for the rest of this request, once. See
    // `propagation_priority`.
    crate::propagation_priority::claim_once();

    let Some(handle) = zend_helpers::arg_object_handle(execute_data, 0) else {
        return;
    };

    let mut headers = CURL_HEADERS
        .with(|map| map.borrow().get(&handle).cloned())
        .unwrap_or_default();
    // The request's inbound `tracestate`/`baggage` ride along VERBATIM. They were
    // captured next to the traceparent (see `lib.rs::start_request`) and never
    // parsed — pass-through is the whole contract.
    let (tracestate, baggage) = REQUEST_CONTEXT.with(|ctx| {
        ctx.borrow()
            .as_ref()
            .map(|c| (c.tracestate.clone(), c.baggage.clone()))
            .unwrap_or((None, None))
    });
    merge_propagation_headers(
        &mut headers,
        traceparent,
        tracestate.as_deref(),
        baggage.as_deref(),
    );

    zend_helpers::call_curl_setopt_httpheader(execute_data, &headers);

    // The merged list is now the handle's effective header set.
    CURL_HEADERS.with(|map| {
        map.borrow_mut().insert(handle, headers);
    });
}

/// Merge the trace-propagation headers into an application's own curl header list.
///
/// `traceparent` always lands. `tracestate` is forwarded whenever the inbound
/// request carried one, because W3C Trace Context REQUIRES a participant that
/// forwards `traceparent` to also forward `tracestate` it does not understand —
/// the vendor entries in it belong to OTHER tracers sharing this trace, and
/// dropping them severs their correlation. `baggage` follows the same
/// forward-as-is contract (its own W3C spec).
///
/// Dedupe mirrors the traceparent idempotence rule that was always here: each
/// header we are about to add first evicts any earlier spelling of itself (a
/// retried handle, or an application that forwarded the inbound headers by
/// hand). A header we have NO value for is left untouched — an application's
/// own `tracestate` is not ours to remove.
fn merge_propagation_headers(
    headers: &mut Vec<String>,
    traceparent: &str,
    tracestate: Option<&str>,
    baggage: Option<&str>,
) {
    // Idempotence: never stack multiple traceparent headers on retried handles.
    headers.retain(|h| !h.to_ascii_lowercase().starts_with("traceparent:"));
    headers.push(format!("traceparent: {traceparent}"));
    if let Some(tracestate) = tracestate {
        headers.retain(|h| !h.to_ascii_lowercase().starts_with("tracestate:"));
        headers.push(format!("tracestate: {tracestate}"));
    }
    if let Some(baggage) = baggage {
        headers.retain(|h| !h.to_ascii_lowercase().starts_with("baggage:"));
        headers.push(format!("baggage: {baggage}"));
    }
}

#[cfg(feature = "zend-observer")]
thread_local! {
    static PENDING_TRACEPARENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Called from the PHP-registered `chronos_pending_traceparent()` function
/// to retrieve the pending traceparent value.
pub fn take_pending_traceparent() -> Option<String> {
    #[cfg(feature = "zend-observer")]
    {
        PENDING_TRACEPARENT.with(|tp| tp.borrow_mut().take())
    }
    #[cfg(not(feature = "zend-observer"))]
    {
        None
    }
}

#[cfg(feature = "zend-observer")]
pub(crate) mod zend_helpers {
    use std::ffi::CStr;

    pub unsafe fn function_name(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<String> {
        let func = (*execute_data).func;
        if func.is_null() {
            return None;
        }
        let func_name = (*func).common.function_name;
        if func_name.is_null() {
            return None;
        }
        let name = CStr::from_ptr((*func_name).val.as_ptr()).to_string_lossy();

        let scope = (*func).common.scope;
        if !scope.is_null() {
            let class_name = (*scope).name;
            if !class_name.is_null() {
                let cls = CStr::from_ptr((*class_name).val.as_ptr()).to_string_lossy();
                return Some(format!("{cls}::{name}"));
            }
        }
        Some(name.into_owned())
    }

    /// The runtime's own handle for the observed function, as an opaque `usize`.
    ///
    /// THE interning key. `zend_function` is allocated once per function and lives as
    /// long as the process (op arrays and internal function structs are engine-owned and
    /// cached), which is exactly the property an interner needs — and it is the same key
    /// the Zend observer factory itself caches its verdict on, so a hit here is a hit
    /// there. `0` means "no handle", never a valid function.
    ///
    /// Returned as a `usize` rather than a pointer so the interner can live in a module
    /// with no Zend types and stay unit-testable without PHP.
    pub unsafe fn function_ptr(execute_data: *mut ext_php_rs::ffi::zend_execute_data) -> usize {
        if execute_data.is_null() {
            return 0;
        }
        (*execute_data).func as usize
    }

    /// Everything the interner needs about a function, from ONE walk of its
    /// `zend_function`: canonical name, module, defining file, declaring line, and
    /// whether it is engine-internal.
    ///
    /// Called only on an interner MISS, which is what makes it affordable to be this
    /// thorough. Every pointer is null-checked before dereference — `panic = "abort"` is
    /// set for the release profile, so an unwrap here is a worker crash, not an error.
    pub unsafe fn function_facts(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<crate::deterministic::FunctionFacts> {
        if execute_data.is_null() {
            return None;
        }
        let func = (*execute_data).func;
        if func.is_null() {
            return None;
        }
        let name = function_name(execute_data)?;
        let internal = (*func).type_ != ext_php_rs::ffi::ZEND_USER_FUNCTION as u8;
        // `module` uses the same vocabulary the sampler's stack walk writes to
        // `SampleFrame::module`, so a counted row and a sampled frame describe their
        // origin identically.
        let module: std::rc::Rc<str> = if internal {
            std::rc::Rc::from("internal")
        } else {
            std::rc::Rc::from("php")
        };
        let (file, line) = if internal {
            (std::rc::Rc::from(""), 0)
        } else {
            let file: std::rc::Rc<str> = match defining_file(execute_data) {
                Some(file) => std::rc::Rc::from(file.as_str()),
                None => std::rc::Rc::from(""),
            };
            (file, (*func).op_array.line_start)
        };
        Some(crate::deterministic::FunctionFacts {
            name,
            origin: crate::deterministic::FunctionOrigin { module, file, line },
            internal,
        })
    }

    /// The compiled-file path of a userland function's op array — where the function is
    /// DEFINED. `None` for internal functions, and for user functions with no filename
    /// (eval'd code), which the path rules treat as unknown rather than as application
    /// code.
    pub unsafe fn defining_file(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<String> {
        let func = (*execute_data).func;
        if func.is_null() || (*func).type_ != ext_php_rs::ffi::ZEND_USER_FUNCTION as u8 {
            return None;
        }
        let filename = (*func).op_array.filename;
        if filename.is_null() {
            return None;
        }
        let file = CStr::from_ptr((*filename).val.as_ptr()).to_string_lossy();
        if file.is_empty() {
            None
        } else {
            Some(file.into_owned())
        }
    }

    /// True for engine-internal functions (builtins and extension methods),
    /// false for userland-defined PHP code.
    pub unsafe fn is_internal_function(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> bool {
        let func = (*execute_data).func;
        if func.is_null() {
            return true;
        }
        (*func).type_ != ext_php_rs::ffi::ZEND_USER_FUNCTION as u8
    }

    /// True when an exception is currently propagating in the executor.
    pub unsafe fn exception_pending() -> bool {
        !ext_php_rs::ffi::executor_globals.exception.is_null()
    }

    /// Identity of a thrown object, read at the moment `zend_throw_exception_hook` fires —
    /// class name plus whichever of `message`/`code`/`file`/`line` the `Throwable` actually
    /// set (a userland class that skips `parent::__construct()` can leave any of them unset).
    pub struct ExceptionIdentity {
        pub class: String,
        pub message: Option<String>,
        pub code: Option<String>,
        pub file: Option<String>,
        pub line: Option<String>,
    }

    /// Class name of a thrown object, read the same way as a call's own scope in
    /// `function_name` above: `zend_class_entry.name` is a `zend_string`.
    unsafe fn exception_class_name(exception: *mut ext_php_rs::ffi::zend_object) -> Option<String> {
        object_class_name(exception)
    }

    /// The RUNTIME class of ANY `zend_object` — `object->ce->name`, read the
    /// same way `function_name` reads a call's declaring scope, except this
    /// reads the OBJECT's own class entry rather than the FUNCTION's.
    ///
    /// That distinction is the entire reason this exists: `function_name`'s
    /// `(*func).common.scope` is the method's DECLARING class — for an
    /// inherited method (`serializeToString`, declared once on
    /// `Google\Protobuf\Internal\Message` and never overridden by a generated
    /// DTO) that is the SAME base class for every subclass, which is exactly
    /// what makes it usable as a stable name-table key in `observe_policy`.
    /// But GAP 1's bank needs the OPPOSITE fact — `$this`'s actual class
    /// (`QlsProtocol\Shared\Webhook`) — and only a read of the object's own
    /// `ce` (not the function's `scope`) gives that. Same null-checked walk,
    /// same `to_string_lossy` (a PHP class name is a `zend_string`, not
    /// guaranteed valid UTF-8, and a lossy class name is still enough to bank
    /// under — worst case one message never matches, never a wrong class).
    pub unsafe fn object_class_name(object: *mut ext_php_rs::ffi::zend_object) -> Option<String> {
        if object.is_null() {
            return None;
        }
        let ce = (*object).ce;
        if ce.is_null() {
            return None;
        }
        let class_name = (*ce).name;
        if class_name.is_null() {
            return None;
        }
        Some(
            CStr::from_ptr((*class_name).val.as_ptr())
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Read one of a `Throwable`'s own properties (`message`, `code`, `file`, `line`) at
    /// throw time via `zend_read_property`. `silent` suppresses the engine's "undefined
    /// property" notice — every `Throwable` declares these, but a userland class that
    /// overrides the constructor without calling `parent::__construct()` can leave them
    /// unset. Reuses `zval_to_owned_string` below, which already handles both the string
    /// (`message`, `file`) and long (`code`, `line`) property types.
    unsafe fn read_exception_property(
        exception: *mut ext_php_rs::ffi::zend_object,
        name: &str,
    ) -> Option<String> {
        let ce = (*exception).ce;
        if ce.is_null() {
            return None;
        }
        let name = std::ffi::CString::new(name).ok()?;
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = ext_php_rs::ffi::zend_read_property(
            ce,
            exception,
            name.as_ptr(),
            name.as_bytes().len(),
            true,
            rv.as_mut_ptr(),
        );
        if prop.is_null() {
            return None;
        }
        zval_to_owned_string(prop)
    }

    /// # Safety
    /// `exception` must be a valid pointer to a `Throwable`'s `zend_object`, as passed to
    /// `zend_throw_exception_hook`.
    pub unsafe fn exception_identity(
        exception: *mut ext_php_rs::ffi::zend_object,
    ) -> Option<ExceptionIdentity> {
        let class = exception_class_name(exception)?;
        Some(ExceptionIdentity {
            class,
            message: read_exception_property(exception, "message"),
            code: read_exception_property(exception, "code"),
            file: read_exception_property(exception, "file"),
            line: read_exception_property(exception, "line"),
        })
    }

    /// Pointer to the i-th (0-based) argument zval of the observed call.
    /// ZEND_CALL_FRAME_SLOT: zend_execute_data is zval-aligned, so advancing one
    /// zend_execute_data lands exactly on the first argument slot.
    unsafe fn arg_zval(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> *mut ext_php_rs::ffi::zval {
        let argc = (*execute_data).This.u2.num_args as usize;
        if index >= argc {
            return std::ptr::null_mut();
        }
        (execute_data.add(1) as *mut ext_php_rs::ffi::zval).add(index)
    }

    const IS_NULL: u8 = 1;
    const IS_LONG: u8 = 4;
    const IS_DOUBLE: u8 = 5;
    const IS_STRING: u8 = 6;
    const IS_ARRAY: u8 = 7;
    const IS_OBJECT: u8 = 8;
    const IS_RESOURCE: u8 = 9;
    const IS_REFERENCE: u8 = 10;
    const IS_TRUE: u8 = 3;
    const IS_FALSE: u8 = 2;

    unsafe fn zval_type(zv: *const ext_php_rs::ffi::zval) -> u8 {
        (*zv).u1.v.type_
    }

    /// The correct zval type tag for a `zend_string` this crate just built
    /// with `ext_php_rs_zend_string_init` — `IS_INTERNED_STRING_EX` (NOT
    /// refcounted) when the string is one of PHP's own shared singletons,
    /// `IS_STRING_EX` (refcounted, copyable) otherwise.
    ///
    /// `ext_php_rs_zend_string_init` (`wrapper.c`) special-cases every
    /// request for length 0 or 1: it hands back `zend_empty_string` or the
    /// matching entry of the process-wide `zend_one_char_string[256]` table
    /// instead of allocating — `GC_IMMUTABLE` is how those, and any other
    /// engine-shared string, are told apart from a genuinely fresh one
    /// (mirrors `Zval::set_zend_string`'s own `is_interned` check in
    /// ext-php-rs, which exists for exactly this reason).
    ///
    /// Tagging a shared singleton as refcounted anyway is NOT cosmetic: it
    /// tells every later copy of the zval (a `foreach`, `zend_array_dup`,
    /// this very array's own destruction at the end of the request) to
    /// `GC_ADDREF`/`GC_DELREF` a PROCESS-GLOBAL table entry whose count nothing
    /// outside PHP's own string subsystem is supposed to touch. Confirmed
    /// live: the AMQP wire format's `'S'` (long-string) tuple tag is exactly
    /// one byte, so skipping this check corrupted `zend_interned_strings_dtor`'s
    /// bookkeeping — reproduced as a `zend_mm_heap corrupted` abort at
    /// `php_module_shutdown`, on literally the first real publish this table
    /// ever carried.
    unsafe fn owned_string_type_info(zs: *mut ext_php_rs::ffi::zend_string) -> u32 {
        if (*zs).gc.u.type_info & ext_php_rs::ffi::GC_IMMUTABLE != 0 {
            ext_php_rs::ffi::IS_INTERNED_STRING_EX
        } else {
            ext_php_rs::ffi::IS_STRING_EX
        }
    }

    pub unsafe fn arg_object_handle(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> Option<u32> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() || zval_type(zv) != IS_OBJECT {
            return None;
        }
        let obj = (*zv).value.obj;
        if obj.is_null() {
            return None;
        }
        Some((*obj).handle)
    }

    /// The object at argument `index`, when it is one — the pointer sibling
    /// of [`arg_object_handle`], for callers that go on to read the
    /// argument's own properties (amqplib's `$message` at `basic_deliver`'s
    /// begin, NATS's `$message`/`$handler` at `sendMessage`/`processMsg`'s
    /// begin). One `IS_REFERENCE` deref, same as every other arg reader here.
    pub unsafe fn arg_object(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> Option<*mut ext_php_rs::ffi::zend_object> {
        let mut zv = arg_zval(execute_data, index);
        if zv.is_null() {
            return None;
        }
        if zval_type(zv) == IS_REFERENCE {
            zv = std::ptr::addr_of_mut!((*(*zv).value.ref_).val);
        }
        if zval_type(zv) != IS_OBJECT {
            return None;
        }
        let obj = (*zv).value.obj;
        if obj.is_null() {
            return None;
        }
        Some(obj)
    }

    /// Whether argument `index` is present at all and holds an object —
    /// distinguishes "not passed" (an omitted trailing default) from
    /// "passed but not an object" without needing a second call, for a
    /// caller like NATS's `processMsg` that must tell a `Queue` handler
    /// apart from a real callable/closure using only the argument's TYPE
    /// (a callable can be a string, array, or object — this predicate only
    /// answers the object case one caller actually asks about).
    pub unsafe fn arg_is_instance_of(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
        class_name: &str,
    ) -> bool {
        let Some(object) = arg_object(execute_data, index) else {
            return false;
        };
        object_instance_of(object, class_name)
    }

    /// PHP `instanceof` for CLASS inheritance: true when the object's own
    /// class, or any ancestor up its `parent` chain, is named `class_name`.
    ///
    /// The parent WALK (rather than one exact name compare) is what the NATS
    /// contract's own `instanceof` wording requires: a userland
    /// `MyQueue extends \Basis\Nats\Queue` handed to `Client::subscribe` is
    /// still a buffering `Queue` — an exact-name compare would misread it as
    /// a real callable and open a message-scoped request around a call that
    /// runs no application code at all. Interfaces are deliberately NOT
    /// walked: every class this file tests for (`Queue`, `Message\Publish`,
    /// `Message\Subscribe`) is a concrete class, and the interface table is
    /// a separate list this read has no need to touch.
    ///
    /// `ce.__bindgen_anon_1` is the engine's `parent`/`parent_name` union;
    /// `parent` (the linked ce pointer) is the live member for any class an
    /// OBJECT exists of — an unlinked class cannot be instantiated — the
    /// same guarantee `object_class_name` already leans on for `ce` itself.
    /// The walk is depth-capped: PHP's own inheritance has no cycles, so the
    /// cap is unreachable, but a corrupted pointer looping forever inside an
    /// observer handler would hang the worker where the cap turns it into a
    /// plain `false`.
    pub unsafe fn object_instance_of(
        object: *mut ext_php_rs::ffi::zend_object,
        class_name: &str,
    ) -> bool {
        if object.is_null() {
            return false;
        }
        let mut ce = (*object).ce;
        for _ in 0..64 {
            if ce.is_null() {
                return false;
            }
            let name = (*ce).name;
            if !name.is_null() {
                let candidate = CStr::from_ptr((*name).val.as_ptr()).to_string_lossy();
                if candidate == class_name {
                    return true;
                }
            }
            ce = (*ce).__bindgen_anon_1.parent;
        }
        false
    }

    /// Handle id of the object a METHOD call is invoked on (`$this`) — how the
    /// prepared-statement map is keyed at `mysqli_stmt::execute` /
    /// `SQLite3Stmt::execute` time, matching the key banked off the prepare call's
    /// return value. `None` for a plain function call or a static call, which
    /// have no bound object.
    pub unsafe fn this_object_handle(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<u32> {
        if execute_data.is_null() {
            return None;
        }
        let this = std::ptr::addr_of!((*execute_data).This) as *const ext_php_rs::ffi::zval;
        if zval_type(this) != IS_OBJECT {
            return None;
        }
        let obj = (*this).value.obj;
        if obj.is_null() {
            return None;
        }
        Some((*obj).handle)
    }

    /// Whether a return value is the boolean `true` — how `mysqli_stmt::prepare`
    /// reports success. A null retval pointer (a call unwinding on an exception)
    /// reads as failure, which is the conservative answer for a banking decision.
    pub unsafe fn retval_is_true(retval: *mut ext_php_rs::ffi::zval) -> bool {
        !retval.is_null() && zval_type(retval) == IS_TRUE
    }

    /// Handle id of an object RETURN VALUE (`mysqli::prepare` returning a
    /// `mysqli_stmt`). `None` when the call returned anything else — a failed
    /// prepare returns `false`, and there is nothing to key against then.
    pub unsafe fn retval_object_handle(retval: *mut ext_php_rs::ffi::zval) -> Option<u32> {
        if retval.is_null() || zval_type(retval) != IS_OBJECT {
            return None;
        }
        let obj = (*retval).value.obj;
        if obj.is_null() {
            return None;
        }
        Some((*obj).handle)
    }

    /// The object RETURN VALUE itself, when there is one — the pointer sibling
    /// of [`retval_object_handle`], for callers that go on to read the object's
    /// properties (`MethodBasicConsumeOkFrame::$consumerTag`).
    pub unsafe fn retval_object(
        retval: *mut ext_php_rs::ffi::zval,
    ) -> Option<*mut ext_php_rs::ffi::zend_object> {
        if retval.is_null() || zval_type(retval) != IS_OBJECT {
            return None;
        }
        let obj = (*retval).value.obj;
        if obj.is_null() {
            return None;
        }
        Some(obj)
    }

    /// The object a METHOD call is invoked on (`$this`) — the pointer sibling of
    /// [`this_object_handle`], for the messaging paths that read the receiver's
    /// properties (a Bunny client's `$options`, a channel's frames).
    pub unsafe fn this_object(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<*mut ext_php_rs::ffi::zend_object> {
        if execute_data.is_null() {
            return None;
        }
        let this = std::ptr::addr_of!((*execute_data).This) as *const ext_php_rs::ffi::zval;
        if zval_type(this) != IS_OBJECT {
            return None;
        }
        let obj = (*this).value.obj;
        if obj.is_null() {
            return None;
        }
        Some(obj)
    }

    /// Read one property of ANY object, protected and private included.
    ///
    /// `zend_read_property` sets `EG(fake_scope)` to the class entry it is
    /// handed for the duration of the read, so passing the object's OWN ce
    /// grants full visibility — the same mechanism `read_exception_property`
    /// above has always relied on (a Throwable's `message` is protected).
    /// Visibility binds USERLAND readers, not the engine, and this fact is what
    /// makes the native messaging path strictly better than the bespoke bridge:
    /// `Bunny\AbstractClient::$options` has no getter, so PHP-side telemetry had
    /// to take the vhost as a parameter and could never see the broker host at
    /// all. `silent = true` suppresses the undefined-property notice — a missing
    /// property is an absent fact, never an error the application sees.
    ///
    /// One `IS_REFERENCE` level is dereferenced. The returned pointer is valid
    /// only until the next engine call — every caller copies out immediately.
    unsafe fn read_object_property(
        object: *mut ext_php_rs::ffi::zend_object,
        name: &str,
        rv: *mut ext_php_rs::ffi::zval,
    ) -> *mut ext_php_rs::ffi::zval {
        if object.is_null() {
            return std::ptr::null_mut();
        }
        let ce = (*object).ce;
        if ce.is_null() {
            return std::ptr::null_mut();
        }
        let Ok(name) = std::ffi::CString::new(name) else {
            return std::ptr::null_mut();
        };
        let mut prop = ext_php_rs::ffi::zend_read_property(
            ce,
            object,
            name.as_ptr(),
            name.as_bytes().len(),
            true,
            rv,
        );
        if !prop.is_null() && zval_type(prop) == IS_REFERENCE {
            prop = std::ptr::addr_of_mut!((*(*prop).value.ref_).val);
        }
        prop
    }

    /// A property as a bounded owned string (string/long/double/bool zvals, the
    /// same coercions `zval_to_owned_string` has always made). `None` for a
    /// missing property or any other type.
    pub unsafe fn object_property_string(
        object: *mut ext_php_rs::ffi::zend_object,
        name: &str,
        max: usize,
    ) -> Option<String> {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = read_object_property(object, name, rv.as_mut_ptr());
        if prop.is_null() {
            return None;
        }
        zval_to_owned_string(prop).map(|mut value| {
            if value.len() > max {
                let mut end = max;
                while end > 0 && !value.is_char_boundary(end) {
                    end -= 1;
                }
                value.truncate(end);
            }
            value
        })
    }

    /// A string property's RAW BYTES — for payloads (`Bunny\Protocol\Buffer::$buffer`),
    /// where the lossy UTF-8 conversion of `zval_to_owned_string` would corrupt a
    /// protobuf body before the text-or-base64 decision ever ran. `None` for
    /// anything that is not a string.
    pub unsafe fn object_property_bytes(
        object: *mut ext_php_rs::ffi::zend_object,
        name: &str,
    ) -> Option<Vec<u8>> {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = read_object_property(object, name, rv.as_mut_ptr());
        if prop.is_null() || zval_type(prop) != IS_STRING {
            return None;
        }
        let s = (*prop).value.str_;
        if s.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts((*s).val.as_ptr() as *const u8, (*s).len).to_vec())
    }

    /// A boolean property. `None` for a missing property or any other type, so a
    /// caller's default is its own decision.
    pub unsafe fn object_property_bool(
        object: *mut ext_php_rs::ffi::zend_object,
        name: &str,
    ) -> Option<bool> {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = read_object_property(object, name, rv.as_mut_ptr());
        if prop.is_null() {
            return None;
        }
        match zval_type(prop) {
            IS_TRUE => Some(true),
            IS_FALSE => Some(false),
            _ => None,
        }
    }

    /// An object-typed property, or `None` when it is null/absent/another type —
    /// how the delivery scope walks `channel->deliverFrame`, `->headerFrame`,
    /// `->bodyBuffer` and `->client`.
    pub unsafe fn object_property_object(
        object: *mut ext_php_rs::ffi::zend_object,
        name: &str,
    ) -> Option<*mut ext_php_rs::ffi::zend_object> {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = read_object_property(object, name, rv.as_mut_ptr());
        if prop.is_null() || zval_type(prop) != IS_OBJECT {
            return None;
        }
        let inner = (*prop).value.obj;
        if inner.is_null() {
            return None;
        }
        Some(inner)
    }

    /// Whether an array-typed property has a given string key — Bunny's own
    /// `isset($this->deliverCallbacks[$consumerTag])` guard, mirrored: a message
    /// Bunny would drop must open no request.
    pub unsafe fn object_property_array_has_str_key(
        object: *mut ext_php_rs::ffi::zend_object,
        name: &str,
        key: &str,
    ) -> bool {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = read_object_property(object, name, rv.as_mut_ptr());
        if prop.is_null() || zval_type(prop) != IS_ARRAY {
            return false;
        }
        let arr = (*prop).value.arr;
        if arr.is_null() {
            return false;
        }
        !ext_php_rs::ffi::zend_hash_str_find(
            arr as *const ext_php_rs::ffi::HashTable,
            key.as_ptr().cast(),
            key.len(),
        )
        .is_null()
    }

    /// One string-keyed entry of an array-typed property, as a bounded string —
    /// how a Bunny client's protected `$options['vhost'|'host'|'port']` is read.
    /// Scalar coercions match `zval_to_owned_string` (the port is a long).
    pub unsafe fn object_property_table_string(
        object: *mut ext_php_rs::ffi::zend_object,
        name: &str,
        key: &str,
        max: usize,
    ) -> Option<String> {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = read_object_property(object, name, rv.as_mut_ptr());
        if prop.is_null() || zval_type(prop) != IS_ARRAY {
            return None;
        }
        let arr = (*prop).value.arr;
        if arr.is_null() {
            return None;
        }
        let value = ext_php_rs::ffi::zend_hash_str_find(
            arr as *const ext_php_rs::ffi::HashTable,
            key.as_ptr().cast(),
            key.len(),
        );
        if value.is_null() {
            return None;
        }
        zval_to_owned_string(value).map(|mut text| {
            if text.len() > max {
                let mut end = max;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            text
        })
    }

    /// Arg `index` as RAW BYTES, strings only — the publish body, which must not
    /// pass through a lossy UTF-8 conversion before the text-or-base64 decision
    /// (a protobuf payload would be corrupted). A non-string body (Bunny accepts
    /// whatever it can append to a buffer) yields `None`: it contributes no size
    /// and no payload rather than a guess at one.
    pub unsafe fn arg_string_bytes(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> Option<Vec<u8>> {
        let mut zv = arg_zval(execute_data, index);
        if zv.is_null() {
            return None;
        }
        if zval_type(zv) == IS_REFERENCE {
            zv = std::ptr::addr_of_mut!((*(*zv).value.ref_).val);
        }
        if zval_type(zv) != IS_STRING {
            return None;
        }
        let s = (*zv).value.str_;
        if s.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts((*s).val.as_ptr() as *const u8, (*s).len).to_vec())
    }

    /// One string-keyed entry of an ARRAY argument, as a bounded string — how
    /// the publish begin reads the caller's own `content-type` and `type` out of
    /// the headers argument. `None` when the arg is not an array or the key is
    /// absent or non-scalar.
    pub unsafe fn arg_array_str_key_string(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
        key: &str,
        max: usize,
    ) -> Option<String> {
        let mut zv = arg_zval(execute_data, index);
        if zv.is_null() {
            return None;
        }
        if zval_type(zv) == IS_REFERENCE {
            zv = std::ptr::addr_of_mut!((*(*zv).value.ref_).val);
        }
        if zval_type(zv) != IS_ARRAY {
            return None;
        }
        let arr = (*zv).value.arr;
        if arr.is_null() {
            return None;
        }
        let value = ext_php_rs::ffi::zend_hash_str_find(
            arr as *const ext_php_rs::ffi::HashTable,
            key.as_ptr().cast(),
            key.len(),
        );
        if value.is_null() {
            return None;
        }
        zval_to_owned_string(value).map(|mut text| {
            if text.len() > max {
                let mut end = max;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            text
        })
    }

    /// Write string entries into an ARRAY argument of the observed call, with
    /// add-if-absent semantics per key — the delicate half of native messaging
    /// publish observation, and the crate's only argument WRITE.
    ///
    /// The rules, each guarding a real corruption:
    ///
    /// 1. **The slot must be an actually-passed argument** (`index < num_args`,
    ///    the same bound `arg_zval` enforces). Never bump `num_args`, never
    ///    write an unpassed slot: `ZEND_RECV_INIT`'s default-assignment
    ///    behaviour against a pre-written slot is PHP-version-sensitive, and at
    ///    the chosen observation point (`Bunny\AbstractClient::publish`, which
    ///    Bunny's own chain always calls with all seven arguments positionally)
    ///    the case only exists for direct `Client::publish` callers using
    ///    defaults — who then get a warn-once line, not a crash.
    /// 2. **One `IS_REFERENCE` deref** (by-value params never keep one after
    ///    SEND; defensive only), then the slot must hold an array.
    /// 3. **Copy-on-write**: a shared (`refcount > 1`) or immutable
    ///    (`GC_IMMUTABLE` — the engine's shared empty-array constant, which a
    ///    `[]` default produces, and the COMMON case here) array is
    ///    `zend_array_dup`'d; the old slot value is released and the dup
    ///    (refcount 1) written in. This is what guarantees the application's own
    ///    `$headers` variable is never observably mutated — instrumentation must
    ///    not change what the app publishes OR what it holds. Only a
    ///    refcount-1, mutable array is edited in place.
    /// 4. **Add-if-absent, per key, atomically**: an existing key is KEPT — a
    ///    caller-supplied `traceparent` always wins and ours is simply not
    ///    added; same independently for every other entry. Native never
    ///    validates or rewrites a caller's value.
    ///
    /// No PHP callback is involved (pure hash writes), so unlike
    /// `inject_curl_traceparent` there is no re-entrancy ordering constraint
    /// with `push_frame` — noted because the curl path's constraint is easy to
    /// assume by analogy.
    ///
    /// `Err` carries the warn-once reason; on any error nothing was written and
    /// the caller's span is still recorded (recording a span whose id is not on
    /// the wire breaks nothing — the promise runs the other way).
    ///
    /// # Safety
    /// Called from the Zend observer begin handler with a valid execute_data.
    pub unsafe fn inject_array_entries(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
        entries: &[(&str, String)],
    ) -> Result<(), &'static str> {
        let argc = (*execute_data).This.u2.num_args as usize;
        if index >= argc {
            return Err("headers argument not passed");
        }
        let mut slot = (execute_data.add(1) as *mut ext_php_rs::ffi::zval).add(index);
        if zval_type(slot) == IS_REFERENCE {
            slot = std::ptr::addr_of_mut!((*(*slot).value.ref_).val);
        }
        if zval_type(slot) != IS_ARRAY {
            return Err("headers argument is not an array");
        }
        let mut arr = (*slot).value.arr;
        if arr.is_null() {
            return Err("headers argument is not an array");
        }
        let shared = (*arr).gc.refcount > 1
            || ((*arr).gc.u.type_info & ext_php_rs::ffi::GC_IMMUTABLE) != 0;
        if shared {
            let dup = ext_php_rs::ffi::zend_array_dup(arr);
            if dup.is_null() {
                return Err("headers array could not be copied");
            }
            // Release the slot's old value (a no-op for the immutable empty
            // array, a refcount decrement for a shared one), then hand the dup —
            // refcount 1, mutable — to the slot.
            ext_php_rs::ffi::zval_ptr_dtor(slot);
            (*slot).value.arr = dup;
            (*slot).u1.type_info = ext_php_rs::ffi::IS_ARRAY_EX;
            arr = dup;
        }
        for (key, value) in entries {
            // Caller wins, per key: an existing entry is kept untouched.
            if !ext_php_rs::ffi::zend_hash_str_find(
                arr as *const ext_php_rs::ffi::HashTable,
                key.as_ptr().cast(),
                key.len(),
            )
            .is_null()
            {
                continue;
            }
            let zs = ext_php_rs::ffi::ext_php_rs_zend_string_init(
                value.as_ptr().cast(),
                value.len(),
                false,
            );
            if zs.is_null() {
                continue;
            }
            let mut entry: ext_php_rs::ffi::zval = std::mem::zeroed();
            entry.value.str_ = zs;
            entry.u1.type_info = owned_string_type_info(zs);
            let inserted = ext_php_rs::ffi::zend_hash_str_update(
                arr,
                key.as_ptr().cast(),
                key.len(),
                &mut entry,
            );
            // `zend_hash_str_update` copied the zval into the bucket: the
            // bucket owns `zs` now. `entry` must NOT run its Drop —
            // ext-php-rs's `Zval` Drop releases the held string, which would
            // free a string the array still references (a use-after-free that
            // corrupts the Zend arena; found live as php-fpm SIGSEGVs).
            std::mem::forget(entry);
            if inserted.is_null() {
                // The insert was refused; the string is ours to release.
                ext_php_rs::ffi::ext_php_rs_zend_string_release(zs);
            }
        }
        Ok(())
    }

    pub unsafe fn arg_long(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> Option<i64> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() || zval_type(zv) != IS_LONG {
            return None;
        }
        Some((*zv).value.lval)
    }

    unsafe fn zval_to_owned_string(zv: *const ext_php_rs::ffi::zval) -> Option<String> {
        match zval_type(zv) {
            IS_STRING => {
                let s = (*zv).value.str_;
                if s.is_null() {
                    return None;
                }
                let len = (*s).len;
                let ptr = (*s).val.as_ptr() as *const u8;
                Some(String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned())
            }
            IS_LONG => Some((*zv).value.lval.to_string()),
            IS_DOUBLE => Some((*zv).value.dval.to_string()),
            IS_TRUE => Some("true".to_owned()),
            IS_FALSE => Some("false".to_owned()),
            _ => None,
        }
    }

    /// A scalar retval as a bounded string (for DST recordings). Non-scalars yield None.
    pub unsafe fn scalar_to_string(zv: *mut ext_php_rs::ffi::zval) -> Option<String> {
        if zv.is_null() {
            return None;
        }
        zval_to_owned_string(zv).map(|mut s| {
            s.truncate(512);
            s
        })
    }

    /// Read arg `index` as a bounded scalar string (payload detail capture).
    pub unsafe fn arg_scalar_string(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
        max: usize,
    ) -> Option<String> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() {
            return None;
        }
        zval_to_owned_string(zv).map(|mut s| {
            if s.len() > max {
                let mut end = max;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
            }
            s
        })
    }

    /// Map a zval's type tag onto the Tier 3 argument vocabulary.
    ///
    /// Anything unrecognised reads as `Null`, which carries a type and no value — the
    /// conservative answer, and never a guess at content.
    unsafe fn argument_type_of(
        zv: *const ext_php_rs::ffi::zval,
    ) -> crate::deterministic::ArgumentType {
        use crate::deterministic::ArgumentType;
        match zval_type(zv) {
            IS_TRUE | IS_FALSE => ArgumentType::Bool,
            IS_LONG => ArgumentType::Int,
            IS_DOUBLE => ArgumentType::Float,
            IS_STRING => ArgumentType::Str,
            IS_ARRAY => ArgumentType::Array,
            IS_OBJECT => ArgumentType::Object,
            IS_RESOURCE => ArgumentType::Resource,
            IS_NULL => ArgumentType::Null,
            // IS_UNDEF, IS_REFERENCE and anything a future PHP adds. Never a guess at
            // content: an unrecognised tag reports a type and no value.
            _ => ArgumentType::Null,
        }
    }

    /// The declared parameter name at `index`, when the runtime exposes one.
    ///
    /// Only read for USER functions: an internal function's `arg_info` is a
    /// `zend_internal_arg_info` whose `name` is a plain `char*`, a different layout at
    /// the same offset, and reading one as the other would be a wild dereference.
    unsafe fn declared_argument_name(
        func: *mut ext_php_rs::ffi::zend_function,
        index: usize,
    ) -> String {
        if func.is_null() || (*func).type_ != ext_php_rs::ffi::ZEND_USER_FUNCTION as u8 {
            return String::new();
        }
        let arg_info = (*func).common.arg_info;
        if arg_info.is_null() || index >= (*func).common.num_args as usize {
            return String::new();
        }
        let info = arg_info.add(index);
        let name = (*info).name;
        if name.is_null() {
            return String::new();
        }
        CStr::from_ptr((*name).val.as_ptr())
            .to_string_lossy()
            .into_owned()
    }

    /// TIER 3: this call's arguments as bounded, typed, redacted records.
    ///
    /// SCALARS ONLY. An object, array or resource records its TYPE and never its value:
    /// "argument 3 was an array" is evidence and leaks nothing, while the array itself is
    /// unbounded application data that ADR 0017 refuses. The refusal lives in
    /// `deterministic::capture_argument`, before redaction, so no reordering of this
    /// function can serialise an object's contents.
    ///
    /// Redaction reuses the collector's existing pattern list, so an `$apiToken`
    /// parameter is masked by exactly the rule that masks an `Authorization` header.
    ///
    /// Returns `(captured, dropped_by_count)`. The caller is responsible for having
    /// checked the manifest allowlist and the Tier 3 arming gate — this function is a
    /// reader, not a policy.
    pub unsafe fn capture_scalar_arguments(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        max_arguments: usize,
        max_bytes: usize,
    ) -> (Vec<crate::deterministic::CapturedArgument>, u32) {
        if execute_data.is_null() {
            return (Vec::new(), 0);
        }
        let func = (*execute_data).func;
        let argc = (*execute_data).This.u2.num_args as usize;
        let taken = argc.min(max_arguments);
        let dropped = u32::try_from(argc.saturating_sub(taken)).unwrap_or(u32::MAX);
        let mut captured = Vec::with_capacity(taken);
        for index in 0..taken {
            let zv = arg_zval(execute_data, index);
            let (argument_type, raw) = if zv.is_null() {
                (crate::deterministic::ArgumentType::Null, None)
            } else {
                let argument_type = argument_type_of(zv);
                let raw = if argument_type.carries_value() {
                    zval_to_owned_string(zv)
                } else {
                    None
                };
                (argument_type, raw)
            };
            let name = declared_argument_name(func, index);
            let redact = crate::http_capture::redacts_identifier(&name);
            captured.push(crate::deterministic::capture_argument(
                u32::try_from(index).unwrap_or(u32::MAX),
                name,
                argument_type,
                raw,
                redact,
                max_bytes,
            ));
        }
        (captured, dropped)
    }

    /// Read arg `index` as a PHP list of strings.
    pub unsafe fn arg_string_array(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
    ) -> Option<Vec<String>> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() || zval_type(zv) != IS_ARRAY {
            return None;
        }
        array_string_values((*zv).value.arr)
    }

    /// Read arg `index` as a PHP array, returning the string-list value at integer key `key`.
    pub unsafe fn arg_array_key_string_array(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        index: usize,
        key: i64,
    ) -> Option<Vec<String>> {
        let zv = arg_zval(execute_data, index);
        if zv.is_null() || zval_type(zv) != IS_ARRAY {
            return None;
        }
        let arr = (*zv).value.arr;
        if arr.is_null() {
            return None;
        }
        let ht = &*(arr as *const ext_php_rs::types::ZendHashTable);
        let value = ht.get_index(key)?;
        let vz = value as *const ext_php_rs::types::Zval as *const ext_php_rs::ffi::zval;
        if zval_type(vz) != IS_ARRAY {
            return None;
        }
        array_string_values((*vz).value.arr)
    }

    unsafe fn array_string_values(arr: *mut ext_php_rs::ffi::zend_array) -> Option<Vec<String>> {
        if arr.is_null() {
            return None;
        }
        let ht = &*(arr as *const ext_php_rs::types::ZendHashTable);
        let mut out = Vec::new();
        for (_key, value) in ht.iter() {
            let vz = value as *const ext_php_rs::types::Zval as *const ext_php_rs::ffi::zval;
            if let Some(s) = zval_to_owned_string(vz) {
                out.push(s);
            }
            if out.len() >= 64 {
                break;
            }
        }
        Some(out)
    }

    /// Call `curl_setopt($ch, CURLOPT_HTTPHEADER, $headers)` on the handle in arg 0
    /// of the observed `curl_exec` call.
    pub unsafe fn call_curl_setopt_httpheader(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
        headers: &[String],
    ) {
        let ch = arg_zval(execute_data, 0);
        if ch.is_null() {
            return;
        }
        let Some(func) = ext_php_rs::zend::Function::try_from_function("curl_setopt") else {
            return;
        };
        let ch_ref = &*(ch as *const ext_php_rs::types::Zval);
        let headers_vec: Vec<String> = headers.to_vec();
        let _ = func.try_call(vec![ch_ref, &super::CURLOPT_HTTPHEADER, &headers_vec]);
    }

    /// The whole `curl_getinfo($ch)` associative array, flattened to strings.
    ///
    /// The one-argument form is used deliberately: asking for each CURLINFO_* constant
    /// individually would mean hard-coding a dozen numeric constants and paying a PHP
    /// call for each, and the constants' *values* have moved between curl releases in
    /// ways the array keys never have.
    pub unsafe fn curl_info(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> std::collections::HashMap<String, String> {
        use ext_php_rs::types::ArrayKey;
        let mut out = std::collections::HashMap::new();
        let ch = arg_zval(execute_data, 0);
        if ch.is_null() {
            return out;
        }
        let Some(func) = ext_php_rs::zend::Function::try_from_function("curl_getinfo") else {
            return out;
        };
        let ch_ref = &*(ch as *const ext_php_rs::types::Zval);
        let Ok(result) = func.try_call(vec![ch_ref]) else {
            return out;
        };
        let Some(table) = result.array() else {
            return out;
        };
        for (key, value) in table.iter() {
            let key = match key {
                ArrayKey::String(k) => k,
                ArrayKey::Str(k) => k.to_owned(),
                _ => continue,
            };
            // curl mixes strings, longs and doubles in one array; the timings are the
            // doubles, so a string-only read would silently drop the whole timeline.
            let value = if let Some(text) = value.str() {
                text.to_owned()
            } else if let Some(number) = value.long() {
                number.to_string()
            } else if let Some(number) = value.double() {
                format!("{number}")
            } else {
                continue;
            };
            out.insert(key, value);
        }
        out
    }

    /// A returned zval as RAW BYTES, string returns only — the
    /// `serializeToString` sibling of [`arg_string_bytes`] above, over a
    /// RETURN VALUE rather than an argument. Same reason as that function:
    /// [`string_retval`] below goes through `Zval::str()`, which is a lossy
    /// UTF-8 read, and a serialized protobuf message is binary — the GAP 1
    /// bank must hash and length-check the WIRE bytes, not a mangled copy of
    /// them, or every non-UTF-8 payload would bank under one identity that
    /// never matches its own publish body.
    pub unsafe fn retval_string_bytes(retval: *mut ext_php_rs::ffi::zval) -> Option<Vec<u8>> {
        let mut zv = retval;
        if zv.is_null() {
            return None;
        }
        if zval_type(zv) == IS_REFERENCE {
            zv = std::ptr::addr_of_mut!((*(*zv).value.ref_).val);
        }
        if zval_type(zv) != IS_STRING {
            return None;
        }
        let s = (*zv).value.str_;
        if s.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts((*s).val.as_ptr() as *const u8, (*s).len).to_vec())
    }

    /// A returned zval as a String, but ONLY when it really is one. `curl_exec`
    /// without CURLOPT_RETURNTRANSFER returns `true`, and coercing that to "1" would
    /// present a one-byte lie as the response body.
    pub unsafe fn string_retval(retval: *mut ext_php_rs::ffi::zval) -> Option<String> {
        if retval.is_null() {
            return None;
        }
        let zv = &*(retval as *const ext_php_rs::types::Zval);
        zv.str()
            .map(std::borrow::ToOwned::to_owned)
            .filter(|s| !s.is_empty())
    }

    /// The URL configured on the curl handle in arg 0 of the observed
    /// `curl_exec` call, via `curl_getinfo($ch, CURLINFO_EFFECTIVE_URL)`.
    pub unsafe fn curl_effective_url(
        execute_data: *mut ext_php_rs::ffi::zend_execute_data,
    ) -> Option<String> {
        const CURLINFO_EFFECTIVE_URL: i64 = 0x0010_0000 + 1;
        let ch = arg_zval(execute_data, 0);
        if ch.is_null() {
            return None;
        }
        let func = ext_php_rs::zend::Function::try_from_function("curl_getinfo")?;
        let ch_ref = &*(ch as *const ext_php_rs::types::Zval);
        let result = func.try_call(vec![ch_ref, &CURLINFO_EFFECTIVE_URL]).ok()?;
        let url = result.string()?;
        if url.is_empty() {
            return None;
        }
        let mut url = url;
        url.truncate(512);
        Some(url)
    }

    /// Call a zero-argument, non-overridable internal method on `object` and
    /// read its return value as a bounded string — the RdKafka contract's
    /// bridge for `RdKafka\ProducerTopic::getName()`: at `produce`/`producev`,
    /// `$this` is the Topic, and the topic name is neither an argument nor a
    /// declared property (`kafka_topic_object`'s C struct field is invisible
    /// to `zend_read_property`, see the contract's "vhost/cluster/server
    /// address recovery" section) — a native→PHP method call is the only
    /// sound route, and this is safe re-entrancy for the SAME reason
    /// `curl_effective_url` above already calls back into PHP
    /// (`curl_getinfo`) from inside the END trampoline: `getName` matches no
    /// messaging/SQL/cache/network table and isn't userland, so
    /// `observe_policy` attaches no begin/end handlers to it — this call
    /// cannot recurse into our own trampolines or double-push `CALL_FRAMES`.
    ///
    /// `ZendObject::try_call_method` (ext-php-rs `types::object`) is used
    /// rather than a raw `zend_call_known_function` here: `ZendObject` is a
    /// type ALIAS for `ext_php_rs::ffi::zend_object` (not a wrapper), so a
    /// `*mut ffi::zend_object` this module already holds can call it directly
    /// with no cast games beyond the reference itself — the exact
    /// `&*(ptr as *const types::Zval)` idiom `call_curl_setopt_httpheader`
    /// already uses for the analogous `types::Zval` alias.
    ///
    /// `None` for a missing method, a call that returns non-string, or an
    /// empty result — never a fabricated topic name.
    pub unsafe fn call_object_method_string(
        object: *mut ext_php_rs::ffi::zend_object,
        method: &str,
        max: usize,
    ) -> Option<String> {
        if object.is_null() {
            return None;
        }
        let object_ref = &*(object as *const ext_php_rs::types::ZendObject);
        let result = object_ref.try_call_method(method, Vec::new()).ok()?;
        let mut value = result.string()?;
        if value.is_empty() {
            return None;
        }
        if value.len() > max {
            let mut end = max;
            while end > 0 && !value.is_char_boundary(end) {
                end -= 1;
            }
            value.truncate(end);
        }
        Some(value)
    }

    /// One string-keyed entry of `$message->properties['application_headers']`
    /// (amqplib) — the read side of the three-shape header carrier the write
    /// side (`inject_amqp_application_headers`) also handles: absent, a plain
    /// array of `[tag, value]` tuples (the back-compat shape `write_table`
    /// accepts identically to a real `AMQPTable`, and the shape the injector
    /// itself writes), a plain array of BARE scalars (an application that
    /// built the array by hand without the tuple wrapper — tolerated, not
    /// required), or an `AMQPTable` OBJECT (whose own protected `$data` holds
    /// `[int-tag, value]` tuples in the identical layout). Never a userland
    /// method call — `AMQPTable::getNativeData()` would work but is exactly
    /// the call this read path avoids on principle (the contract's
    /// "Schema-name propagation" section).
    pub unsafe fn amqp_application_header(
        message: *mut ext_php_rs::ffi::zend_object,
        key: &str,
        max: usize,
    ) -> Option<String> {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let properties = read_object_property(message, "properties", rv.as_mut_ptr());
        if properties.is_null() || zval_type(properties) != IS_ARRAY {
            return None;
        }
        let properties_arr = (*properties).value.arr;
        if properties_arr.is_null() {
            return None;
        }
        let headers_value = ext_php_rs::ffi::zend_hash_str_find(
            properties_arr as *const ext_php_rs::ffi::HashTable,
            "application_headers".as_ptr().cast(),
            "application_headers".len(),
        );
        if headers_value.is_null() {
            return None;
        }
        match zval_type(headers_value) {
            IS_ARRAY => amqp_read_header_from_array((*headers_value).value.arr, key, max),
            IS_OBJECT => {
                let table_obj = (*headers_value).value.obj;
                if table_obj.is_null() {
                    return None;
                }
                let mut rv2 = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
                let data = read_object_property(table_obj, "data", rv2.as_mut_ptr());
                if data.is_null() || zval_type(data) != IS_ARRAY {
                    return None;
                }
                amqp_read_header_from_array((*data).value.arr, key, max)
            }
            _ => None,
        }
    }

    /// One key's VALUE out of an amqp headers array, tolerating both the
    /// `[tag, value]` tuple shape and a bare scalar (an application that
    /// wrote the array by hand without the tuple wrapper).
    unsafe fn amqp_read_header_from_array(
        arr: *mut ext_php_rs::ffi::zend_array,
        key: &str,
        max: usize,
    ) -> Option<String> {
        if arr.is_null() {
            return None;
        }
        let value = ext_php_rs::ffi::zend_hash_str_find(
            arr as *const ext_php_rs::ffi::HashTable,
            key.as_ptr().cast(),
            key.len(),
        );
        if value.is_null() {
            return None;
        }
        let scalar = if zval_type(value) == IS_ARRAY {
            // Index 1 is the value half of the `[tag, value]` tuple; index 0
            // (the wire-type tag) is uninteresting to a reader.
            let tuple = (*value).value.arr;
            if tuple.is_null() {
                return None;
            }
            ext_php_rs::ffi::zend_hash_index_find(tuple as *const ext_php_rs::ffi::HashTable, 1)
        } else {
            value
        };
        if scalar.is_null() {
            return None;
        }
        zval_to_owned_string(scalar).map(|mut text| {
            if text.len() > max {
                let mut end = max;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            text
        })
    }

    /// Inject propagation/schema entries into
    /// `$msg->properties['application_headers']` (amqplib), preserving
    /// whichever of the three shapes is already there and never making a
    /// userland method call — see the contract's "Injectable vs span-only"
    /// section for why a raw array is exactly as legal a wire value as a real
    /// `AMQPTable` here (`write_table`, `Wire/AMQPWriter.php:369`, branches on
    /// `instanceof AMQPTable` only to pick the wire type-tag alphabet, never
    /// to reject a plain array).
    ///
    /// New entries are written in the LEGACY symbol-char tuple shape
    /// (`['S', value]`, long string — the `$types_080` alphabet
    /// `write_table`'s non-`AMQPTable` branch reads) when the array is fresh
    /// or already a plain array, and in the `AMQPTable`-internal int-tag
    /// shape (`[AMQPAbstractCollection::T_STRING_LONG /* 14 */, value]`) when
    /// appending into an existing `AMQPTable` object's own `$data` — either
    /// tuple shape is a legal wire value; matching whichever is already there
    /// just means an application reading `$msg` back out via its own methods
    /// sees one consistent representation, not a mixed one.
    ///
    /// Add-if-absent per key, same rule as [`inject_array_entries`]: an
    /// existing entry (of EITHER tuple shape) is left untouched.
    ///
    /// # Safety
    /// Called from the Zend observer begin handler with a valid, live `$msg`
    /// object pointer (arg 0 of `AMQPChannel::basic_publish`).
    pub unsafe fn inject_amqp_application_headers(
        message: *mut ext_php_rs::ffi::zend_object,
        entries: &[(&str, String)],
    ) -> Result<(), &'static str> {
        if message.is_null() {
            return Err("message is null");
        }
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let properties = read_object_property(message, "properties", rv.as_mut_ptr());
        if properties.is_null() {
            return Err("properties not readable");
        }
        if zval_type(properties) != IS_ARRAY {
            return Err("properties is not an array");
        }
        let mut properties_arr = (*properties).value.arr;
        if properties_arr.is_null() {
            return Err("properties is not an array");
        }
        let props_shared = (*properties_arr).gc.refcount > 1
            || ((*properties_arr).gc.u.type_info & ext_php_rs::ffi::GC_IMMUTABLE) != 0;
        if props_shared {
            let dup = ext_php_rs::ffi::zend_array_dup(properties_arr);
            if dup.is_null() {
                return Err("properties array could not be copied");
            }
            ext_php_rs::ffi::zval_ptr_dtor(properties);
            (*properties).value.arr = dup;
            (*properties).u1.type_info = ext_php_rs::ffi::IS_ARRAY_EX;
            properties_arr = dup;
        }

        let existing = ext_php_rs::ffi::zend_hash_str_find(
            properties_arr as *const ext_php_rs::ffi::HashTable,
            "application_headers".as_ptr().cast(),
            "application_headers".len(),
        );

        if existing.is_null() {
            // Case 1 (the common case): absent. A fresh plain array of
            // `[symbol, value]` tuples, inserted as a NEW
            // `application_headers` entry.
            let headers_arr = ext_php_rs::ffi::_zend_new_array(entries.len() as u32);
            if headers_arr.is_null() {
                return Err("headers array could not be allocated");
            }
            for (key, value) in entries {
                amqp_insert_symbol_tuple(headers_arr, key, value);
            }
            let mut entry: ext_php_rs::ffi::zval = std::mem::zeroed();
            entry.value.arr = headers_arr;
            entry.u1.type_info = ext_php_rs::ffi::IS_ARRAY_EX;
            let inserted = ext_php_rs::ffi::zend_hash_str_update(
                properties_arr,
                "application_headers".as_ptr().cast(),
                "application_headers".len(),
                &mut entry,
            );
            // Same UAF-avoidance rule as `inject_array_entries`, applied the
            // same way: forget UNCONDITIONALLY (on success the bucket owns a
            // byte-copy of `entry`'s contents; on refusal the raw pointer is
            // released by hand below). Forgetting only on the success branch
            // — an earlier shape of this code — was a latent double-free: a
            // refused insert would destroy `headers_arr` AND then let
            // `entry`'s Drop release the same array again.
            std::mem::forget(entry);
            if inserted.is_null() {
                ext_php_rs::ffi::zend_array_destroy(headers_arr);
            }
            return Ok(());
        }

        match zval_type(existing) {
            IS_ARRAY => {
                // Case 2: a plain array already (either the injector's own
                // earlier write, or an application-built array). COW-safe
                // append, same tuple shape.
                let mut headers_arr = (*existing).value.arr;
                if headers_arr.is_null() {
                    return Err("application_headers is not an array");
                }
                let shared = (*headers_arr).gc.refcount > 1
                    || ((*headers_arr).gc.u.type_info & ext_php_rs::ffi::GC_IMMUTABLE) != 0;
                if shared {
                    let dup = ext_php_rs::ffi::zend_array_dup(headers_arr);
                    if dup.is_null() {
                        return Err("application_headers array could not be copied");
                    }
                    ext_php_rs::ffi::zval_ptr_dtor(existing);
                    (*existing).value.arr = dup;
                    (*existing).u1.type_info = ext_php_rs::ffi::IS_ARRAY_EX;
                    headers_arr = dup;
                }
                for (key, value) in entries {
                    if !ext_php_rs::ffi::zend_hash_str_find(
                        headers_arr as *const ext_php_rs::ffi::HashTable,
                        key.as_ptr().cast(),
                        key.len(),
                    )
                    .is_null()
                    {
                        continue;
                    }
                    amqp_insert_symbol_tuple(headers_arr, key, value);
                }
                Ok(())
            }
            IS_OBJECT => {
                // Case 3: an `AMQPTable` object. Reach into its own `$data`,
                // same add-if-absent rule, the int-tag tuple shape.
                let table_obj = (*existing).value.obj;
                if table_obj.is_null() {
                    return Err("application_headers object is null");
                }
                inject_amqp_table_data(table_obj, entries)
            }
            _ => Err("application_headers is neither array nor object"),
        }
    }

    /// `AMQPAbstractCollection::T_STRING_LONG` (14) — the tuple-shape tag an
    /// `AMQPTable` object's own `$data` array uses internally.
    const AMQP_T_STRING_LONG: i64 = 14;

    /// Write add-if-absent entries directly into an existing `AMQPTable`
    /// object's own protected `$data` array — one more hop than the plain-
    /// array case, still no userland method call (`AMQPTable::setValue`
    /// would work but is exactly the call the contract's read/write sides
    /// both avoid on principle).
    unsafe fn inject_amqp_table_data(
        table_obj: *mut ext_php_rs::ffi::zend_object,
        entries: &[(&str, String)],
    ) -> Result<(), &'static str> {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let data = read_object_property(table_obj, "data", rv.as_mut_ptr());
        if data.is_null() {
            return Err("AMQPTable data not readable");
        }
        if zval_type(data) != IS_ARRAY {
            return Err("AMQPTable data is not an array");
        }
        let mut data_arr = (*data).value.arr;
        if data_arr.is_null() {
            return Err("AMQPTable data is not an array");
        }
        let shared = (*data_arr).gc.refcount > 1
            || ((*data_arr).gc.u.type_info & ext_php_rs::ffi::GC_IMMUTABLE) != 0;
        if shared {
            let dup = ext_php_rs::ffi::zend_array_dup(data_arr);
            if dup.is_null() {
                return Err("AMQPTable data could not be copied");
            }
            ext_php_rs::ffi::zval_ptr_dtor(data);
            (*data).value.arr = dup;
            (*data).u1.type_info = ext_php_rs::ffi::IS_ARRAY_EX;
            data_arr = dup;
        }
        for (key, value) in entries {
            if !ext_php_rs::ffi::zend_hash_str_find(
                data_arr as *const ext_php_rs::ffi::HashTable,
                key.as_ptr().cast(),
                key.len(),
            )
            .is_null()
            {
                continue;
            }
            amqp_insert_long_tuple(data_arr, key, value, AMQP_T_STRING_LONG);
        }
        Ok(())
    }

    /// Build a `[<'S'>, value]` tuple and insert it at `key` — the legacy
    /// back-compat wire-header shape, add-if-absent (checked by the caller,
    /// which already holds the presence answer from its own lookup — this
    /// function only ever inserts a NEW array, so no additional presence
    /// check is needed here).
    unsafe fn amqp_insert_symbol_tuple(
        arr: *mut ext_php_rs::ffi::zend_array,
        key: &str,
        value: &str,
    ) {
        let tuple = ext_php_rs::ffi::_zend_new_array(2);
        if tuple.is_null() {
            return;
        }
        amqp_tuple_set_string_tag(tuple, "S");
        amqp_tuple_set_value(tuple, value);
        let mut entry: ext_php_rs::ffi::zval = std::mem::zeroed();
        entry.value.arr = tuple;
        entry.u1.type_info = ext_php_rs::ffi::IS_ARRAY_EX;
        let inserted =
            ext_php_rs::ffi::zend_hash_str_update(arr, key.as_ptr().cast(), key.len(), &mut entry);
        // Forget UNCONDITIONALLY, release the raw pointer by hand on refusal
        // — `inject_array_entries`' rule; a conditional forget here would
        // double-free `tuple` on the refusal branch (Drop after destroy).
        std::mem::forget(entry);
        if inserted.is_null() {
            ext_php_rs::ffi::zend_array_destroy(tuple);
        }
    }

    /// Build a `[<long tag>, value]` tuple and insert it at `key` — the
    /// `AMQPTable::$data` internal shape.
    unsafe fn amqp_insert_long_tuple(
        arr: *mut ext_php_rs::ffi::zend_array,
        key: &str,
        value: &str,
        tag: i64,
    ) {
        let tuple = ext_php_rs::ffi::_zend_new_array(2);
        if tuple.is_null() {
            return;
        }
        amqp_tuple_set_long_tag(tuple, tag);
        amqp_tuple_set_value(tuple, value);
        let mut entry: ext_php_rs::ffi::zval = std::mem::zeroed();
        entry.value.arr = tuple;
        entry.u1.type_info = ext_php_rs::ffi::IS_ARRAY_EX;
        let inserted =
            ext_php_rs::ffi::zend_hash_str_update(arr, key.as_ptr().cast(), key.len(), &mut entry);
        // Same unconditional-forget rule as `amqp_insert_symbol_tuple`.
        std::mem::forget(entry);
        if inserted.is_null() {
            ext_php_rs::ffi::zend_array_destroy(tuple);
        }
    }

    /// Index 0 of a 2-element tuple: a single-character STRING tag (the
    /// `$types_080` symbol alphabet, e.g. `'S'` for a long string).
    unsafe fn amqp_tuple_set_string_tag(tuple: *mut ext_php_rs::ffi::zend_array, tag: &str) {
        let zs = ext_php_rs::ffi::ext_php_rs_zend_string_init(tag.as_ptr().cast(), tag.len(), false);
        if zs.is_null() {
            return;
        }
        let mut zv: ext_php_rs::ffi::zval = std::mem::zeroed();
        zv.value.str_ = zs;
        zv.u1.type_info = owned_string_type_info(zs);
        let inserted = ext_php_rs::ffi::zend_hash_index_update(tuple, 0, &mut zv);
        // Same unconditional-forget rule as `inject_array_entries`.
        std::mem::forget(zv);
        if inserted.is_null() {
            ext_php_rs::ffi::ext_php_rs_zend_string_release(zs);
        }
    }

    /// Index 0 of a 2-element tuple: a LONG tag
    /// (`AMQPAbstractCollection::T_*`).
    unsafe fn amqp_tuple_set_long_tag(tuple: *mut ext_php_rs::ffi::zend_array, tag: i64) {
        let mut zv: ext_php_rs::ffi::zval = std::mem::zeroed();
        zv.value.lval = tag;
        zv.u1.type_info = IS_LONG as u32;
        let _ = ext_php_rs::ffi::zend_hash_index_update(tuple, 0, &mut zv);
        // A plain LONG holds no refcounted allocation, so unlike the STRING/
        // ARRAY inserts elsewhere in this file there is nothing a natural
        // drop of `zv` here could double-free — but `forget` it anyway,
        // uniformly, so this function does not depend on a reader knowing
        // that `Zval::drop` happens to be a no-op for `IS_LONG`.
        std::mem::forget(zv);
    }

    /// Index 1 of a 2-element tuple: the value, always a STRING (every
    /// propagation/schema value this crate injects is a string).
    unsafe fn amqp_tuple_set_value(tuple: *mut ext_php_rs::ffi::zend_array, value: &str) {
        let zs =
            ext_php_rs::ffi::ext_php_rs_zend_string_init(value.as_ptr().cast(), value.len(), false);
        if zs.is_null() {
            return;
        }
        let mut zv: ext_php_rs::ffi::zval = std::mem::zeroed();
        zv.value.str_ = zs;
        zv.u1.type_info = owned_string_type_info(zs);
        let inserted = ext_php_rs::ffi::zend_hash_index_update(tuple, 1, &mut zv);
        // Same unconditional-forget rule as `inject_array_entries`.
        std::mem::forget(zv);
        if inserted.is_null() {
            ext_php_rs::ffi::ext_php_rs_zend_string_release(zs);
        }
    }

    /// Write string entries into an object's OWN property array — the NATS
    /// two-hop write (`$message->payload->headers`), adapting
    /// [`inject_array_entries`]'s COW/UAF-safe body from an ARGUMENT slot to
    /// a PROPERTY slot. Every rule that function's docblock states applies
    /// here unchanged; only where the array comes from differs.
    ///
    /// # Safety
    /// Called from the Zend observer begin handler with a valid, live object
    /// pointer (the `Payload` object read off `$message->payload`).
    pub unsafe fn inject_property_array_entries(
        object: *mut ext_php_rs::ffi::zend_object,
        property: &str,
        entries: &[(&str, String)],
    ) -> Result<(), &'static str> {
        if object.is_null() {
            return Err("object is null");
        }
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let slot = read_object_property(object, property, rv.as_mut_ptr());
        if slot.is_null() {
            return Err("headers property not passed");
        }
        if zval_type(slot) != IS_ARRAY {
            return Err("headers property is not an array");
        }
        let mut arr = (*slot).value.arr;
        if arr.is_null() {
            return Err("headers property is not an array");
        }
        let shared = (*arr).gc.refcount > 1
            || ((*arr).gc.u.type_info & ext_php_rs::ffi::GC_IMMUTABLE) != 0;
        if shared {
            let dup = ext_php_rs::ffi::zend_array_dup(arr);
            if dup.is_null() {
                return Err("headers array could not be copied");
            }
            ext_php_rs::ffi::zval_ptr_dtor(slot);
            (*slot).value.arr = dup;
            (*slot).u1.type_info = ext_php_rs::ffi::IS_ARRAY_EX;
            arr = dup;
        }
        for (key, value) in entries {
            if !ext_php_rs::ffi::zend_hash_str_find(
                arr as *const ext_php_rs::ffi::HashTable,
                key.as_ptr().cast(),
                key.len(),
            )
            .is_null()
            {
                continue;
            }
            let zs = ext_php_rs::ffi::ext_php_rs_zend_string_init(
                value.as_ptr().cast(),
                value.len(),
                false,
            );
            if zs.is_null() {
                continue;
            }
            let mut entry: ext_php_rs::ffi::zval = std::mem::zeroed();
            entry.value.str_ = zs;
            entry.u1.type_info = owned_string_type_info(zs);
            let inserted =
                ext_php_rs::ffi::zend_hash_str_update(arr, key.as_ptr().cast(), key.len(), &mut entry);
            std::mem::forget(entry);
            if inserted.is_null() {
                ext_php_rs::ffi::ext_php_rs_zend_string_release(zs);
            }
        }
        Ok(())
    }

    /// A LONG property — the numeric sibling of [`object_property_string`],
    /// for the two RdKafka `Message` facts that need to stay actual integers
    /// rather than their string coercion: `partition` (compared against the
    /// `RD_KAFKA_PARTITION_UA` sentinel by `messaging::kafka_partition_attribute`,
    /// which needs a real `i64` to compare) and `offset`. `None` for a missing
    /// property or any other type, same "caller's own default" rule as
    /// [`object_property_bool`].
    pub unsafe fn object_property_long(
        object: *mut ext_php_rs::ffi::zend_object,
        name: &str,
    ) -> Option<i64> {
        let mut rv = std::mem::MaybeUninit::<ext_php_rs::ffi::zval>::uninit();
        let prop = read_object_property(object, name, rv.as_mut_ptr());
        if prop.is_null() || zval_type(prop) != IS_LONG {
            return None;
        }
        Some((*prop).value.lval)
    }

    /// The number of entries in an array RETURN VALUE — `Basis\Nats\Queue::fetchAll`'s
    /// batch size, read off the call's own retval rather than guessed: the
    /// same "only the return value knows" reasoning `bank_prepared_statement`
    /// already relies on for a prepare's success. `None` when the retval is
    /// not an array at all (should be unreachable for `fetchAll`, whose
    /// declared return type is `array`, but a native reader never assumes
    /// what PHP promises).
    pub unsafe fn retval_array_len(retval: *mut ext_php_rs::ffi::zval) -> Option<u32> {
        if retval.is_null() || zval_type(retval) != IS_ARRAY {
            return None;
        }
        let arr = (*retval).value.arr;
        if arr.is_null() {
            return None;
        }
        Some((*arr).nNumOfElements)
    }
}

fn now_utc() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

fn monotonic_nanos() -> u128 {
    use std::time::Instant;
    thread_local! { static ORIGIN: Instant = Instant::now(); }
    ORIGIN.with(|origin| origin.elapsed().as_nanos())
}

/// The deterministic aggregate's clock, in `u64` nanoseconds.
///
/// THE SAME ORIGIN as [`monotonic_nanos`] above, and that matters: there are three
/// independent `monotonic_nanos` implementations in this crate (here, `sampler.rs`,
/// `lib.rs`), each with its own private origin, so values are NOT comparable across
/// modules. Frame durations are already computed against this one, so the counted
/// aggregate uses it too — a begin stamped by one clock and an end by another would
/// produce durations that are plausible, wrong, and impossible to spot.
///
/// Saturating rather than wrapping on the cast: `u64` nanoseconds is 584 years of
/// process uptime, so the clamp is unreachable, and a wrap would silently produce a
/// negative-looking duration where a clamp produces an obviously stuck one.
fn monotonic_nanos_u64() -> u64 {
    u64::try_from(monotonic_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Dual-purpose stream calls ----------------------------------------

    #[test]
    fn a_remote_target_is_recognised_whatever_the_scheme_case() {
        for target in [
            "http://api.internal/orders",
            "HTTPS://api.internal/orders",
            "ftp://files.internal/report.csv",
            "sftp://files.internal/report.csv",
            "ssh2.sftp://files.internal/report.csv",
        ] {
            assert!(
                is_remote_stream_target(target),
                "{target} is a network fetch"
            );
        }
    }

    #[test]
    fn local_reads_are_never_mistaken_for_network_calls() {
        // These are the calls that were filling the waterfall with 0 ms rows. The
        // allow-list decides, so an unlisted wrapper reads as local.
        for target in [
            "/srv/app/config/routing.yml",
            "config/app.php",
            "php://input",
            "data://text/plain,hello",
            "compress.zlib:///srv/app/cache/x.gz",
            "",
        ] {
            assert!(!is_remote_stream_target(target), "{target} is a local read");
        }
    }

    #[test]
    fn only_the_ambiguous_stream_builtins_have_their_target_inspected() {
        assert!(is_dual_purpose_stream_function("file_get_contents"));
        // curl always crosses the network — nothing to disambiguate, and reading its
        // handle argument as a string would be meaningless.
        assert!(!is_dual_purpose_stream_function("curl_exec"));
        assert!(!is_dual_purpose_stream_function("PDO::query"));
    }

    // --- Excluded paths ----------------------------------------------------

    #[test]
    fn dependency_paths_are_excluded_and_application_paths_are_not() {
        let excluded = DEFAULT_EXCLUDED_PATHS
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>();
        assert!(is_excluded_path(
            "/srv/app/vendor/symfony/Kernel.php",
            &excluded
        ));
        assert!(is_excluded_path(
            "/srv/app/node_modules/x/index.php",
            &excluded
        ));
        assert!(!is_excluded_path(
            "/srv/app/src/Orders/Controller.php",
            &excluded
        ));
        // "vendor" as part of an application's own name is not the dependency tree;
        // the fragments carry their separators for exactly this reason.
        assert!(!is_excluded_path(
            "/srv/app/src/VendorPayouts.php",
            &excluded
        ));
    }

    #[test]
    fn an_unknown_defining_file_is_never_excluded() {
        // Absence of a path is not evidence that the code is a dependency, and
        // guessing would silently drop spans the reader asked for.
        let excluded = vec!["/vendor/".to_owned()];
        assert!(!is_excluded_path("", &excluded));
    }

    #[test]
    fn the_exclude_list_ignores_blank_entries() {
        // A stray comma would otherwise contribute an empty fragment, which
        // `contains` matches against every path — silencing the whole service.
        let parsed = parse_excluded_paths("/vendor/, ,/node_modules/,");
        assert_eq!(
            parsed,
            vec!["/vendor/".to_owned(), "/node_modules/".to_owned()]
        );
        assert!(!is_excluded_path("/srv/app/src/Controller.php", &parsed));
    }

    #[test]
    fn an_empty_exclude_list_excludes_nothing() {
        let parsed = parse_excluded_paths("");
        assert!(parsed.is_empty());
        assert!(!is_excluded_path(
            "/srv/app/vendor/symfony/Kernel.php",
            &parsed
        ));
    }

    // --- Propagation header merging -----------------------------------------

    #[test]
    fn all_three_propagation_headers_are_appended_to_the_applications_own() {
        let mut headers = vec!["Accept: application/json".to_owned()];
        merge_propagation_headers(
            &mut headers,
            "00-abc-def-01",
            Some("vendor=state"),
            Some("userId=1"),
        );
        assert_eq!(
            headers,
            vec![
                "Accept: application/json".to_owned(),
                "traceparent: 00-abc-def-01".to_owned(),
                "tracestate: vendor=state".to_owned(),
                "baggage: userId=1".to_owned(),
            ]
        );
    }

    #[test]
    fn a_retried_handle_never_stacks_duplicate_propagation_headers() {
        let mut headers = vec![
            "TraceParent: 00-old-old-01".to_owned(),
            "tracestate: stale=1".to_owned(),
            "Baggage: stale=1".to_owned(),
        ];
        merge_propagation_headers(&mut headers, "00-new-new-01", Some("fresh=1"), Some("k=v"));
        assert_eq!(
            headers,
            vec![
                "traceparent: 00-new-new-01".to_owned(),
                "tracestate: fresh=1".to_owned(),
                "baggage: k=v".to_owned(),
            ]
        );
    }

    #[test]
    fn an_applications_own_tracestate_survives_when_the_request_carried_none() {
        // We only dedupe a header we are about to re-add. An application that set
        // its own tracestate on a request with no inbound one keeps it.
        let mut headers = vec!["tracestate: mine=1".to_owned()];
        merge_propagation_headers(&mut headers, "00-abc-def-01", None, None);
        assert_eq!(
            headers,
            vec![
                "tracestate: mine=1".to_owned(),
                "traceparent: 00-abc-def-01".to_owned(),
            ]
        );
    }

    // --- Policy ------------------------------------------------------------

    #[test]
    fn engine_builtins_that_are_not_io_never_reach_a_span() {
        assert!(observe_policy("strpos", true).is_none());
        assert!(observe_policy("file_exists", true).is_none());
    }

    #[test]
    fn file_get_contents_still_attaches_handlers_so_its_target_can_be_judged() {
        // The factory's verdict is cached per function by Zend, so the policy here must
        // stay IoSpan; the per-call demotion to ObserveOnly happens in the begin
        // handler, which is the only place the argument exists.
        assert_eq!(
            observe_policy("file_get_contents", true),
            Some(SpanPolicy::IoSpan)
        );
    }
}
