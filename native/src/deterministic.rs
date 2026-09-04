//! Deterministic (counted) profile aggregates — ADR 0029.
//!
//! The `sampler` module answers "where does this service spend its time" by looking at
//! the stack every ~10 ms; its numbers are ESTIMATES and its blind spot is structural
//! (a function that never runs at a safe point is invisible). This module answers
//! "how many times was this function called and how long did it take" EXACTLY, by
//! counting at the begin/end boundaries the Zend observer already crosses on every
//! observed call. The two signals are additive: `.dprofile` never replaces `.profile`,
//! and where they disagree the disagreement is evidence about the sampler's safe-point
//! attribution skew rather than a defect in either.
//!
//! WHY THIS CAN BE ALWAYS ON, when a per-call trace could not. The buffer is keyed by
//! FUNCTION, not by CALL. A request that makes a million calls into two thousand
//! distinct functions costs two thousand rows, so memory is O(distinct functions) and
//! bounded by `max_functions`. A Cachegrind-style per-call trace is O(calls) and could
//! only ever be a forced-request tool. That single property is the whole reason Tier 1
//! defaults ON while every other signal flag in the collector defaults OFF.
//!
//! WHY IT SHOULD LAND NET FASTER THAN THE CODE IT REPLACES. Today EVERY observed call
//! pays two heap allocations before anything is decided: `zend_helpers::function_name`
//! builds a fresh `format!("{class}::{method}")`, and `CallFrame::observe_only` takes an
//! owned copy of it. [`NameInterner`] moves both to once-per-function-per-process by
//! keying on the runtime's own function handle — the same key the Zend observer factory
//! already caches its verdict on. An implementation that adds per-call aggregation
//! WITHOUT interning makes every request slower and must not ship (SPOOL_CONTRACT.md).
//!
//! THE RECURSION RULE, which is the one thing here that is easy to get wrong and hard
//! to notice. `inclusive_nanoseconds` is banked on the OUTERMOST frame only — a
//! function nested fourteen deep inside itself would otherwise report fourteen times
//! its real inclusive time. `exclusive_nanoseconds` is banked at EVERY depth, because
//! self time is not double counted by recursion. `max_recursion_depth` is emitted as
//! the flag on that arithmetic: at `1` the identity
//! `inclusive == exclusive + Σ(callee inclusive)` holds; above `1` it does not, and
//! `inclusive - exclusive` is not child time. A consumer must branch on the field
//! rather than subtract blind.
//!
//! REJECTED ALTERNATIVES, recorded so they are not re-proposed:
//!   * A per-call event stream (Cachegrind / Xdebug trace). O(calls); cannot be
//!     always-on; and the aggregate is what every read actually wants.
//!   * Reusing `CallFrame::start_hrtime` for the deterministic clock. That field feeds
//!     `on_end`'s span-duration and min-duration logic, and an `ObserveOnly` frame must
//!     never become a span. The timing lives on a separate [`FrameTiming`] precisely so
//!     the two paths cannot be confused.
//!   * A parallel push/pop stack of our own. The `ObserveOnly` push in the begin
//!     trampoline is conditional; a second independent stack desyncs on exactly the
//!     requests where that condition is false. This module therefore never owns a
//!     stack — the caller hands back the [`FrameTiming`] it stored on its own
//!     `CallFrame`.
//!   * Capturing all parameters of all functions. Explicitly rejected on ADR 0017
//!     (redaction-first) grounds. Tier 3 is a bounded, allowlisted, scalar-only,
//!     redacted, capped sample and nothing more.
//!
//! Everything in this module is safe Rust with no Zend types: the runtime's function
//! handle arrives as a `usize`, so the whole module compiles and unit-tests under
//! `--no-default-features` while the FFI seam stays in `observer.rs`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Interned handle for one canonical function identity, per process.
pub type FunctionId = u32;

/// Where a function was compiled from. DESCRIPTIVE, never part of the identity — the
/// join key across collector, engine and desktop is the NAME alone (ADR 0029 §4), the
/// same division the flame-graph read already makes with its function → origin map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionOrigin {
    pub module: Rc<str>,
    pub file: Rc<str>,
    pub line: u32,
}

impl Default for FunctionOrigin {
    fn default() -> Self {
        Self {
            module: Rc::from(""),
            file: Rc::from(""),
            line: 0,
        }
    }
}

/// Everything the FFI seam learns about a function in ONE pass over its
/// `zend_function`, resolved only on an interner miss.
///
/// Folded into a single struct rather than three helper calls because the old shape
/// re-dereferenced `(*execute_data).func` once per helper, once per call — and
/// `internal` is read off the very same struct as the name, so splitting them buys
/// nothing and costs a pointer chase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionFacts {
    /// The canonical identity, spelled exactly as the runtime declares it:
    /// `App\Support\slugify`, `App\Orders\Repository::find`, `{main}`.
    pub name: String,
    pub origin: FunctionOrigin,
    /// True for engine builtins and extension methods. Carried alongside the name
    /// because `observe_policy` needs both and both come off one dereference.
    pub internal: bool,
}

/// One interned function, as the hot path sees it.
///
/// Handing this back by value is what makes the begin trampoline allocation-free: the
/// three `Rc` clones are refcount bumps, the `u32`/`bool` are copies, and nothing
/// touches the allocator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternedFunction {
    pub id: FunctionId,
    pub name: Rc<str>,
    pub origin: FunctionOrigin,
    pub internal: bool,
}

impl InternedFunction {
    /// The defining file, or `None` when the runtime reported none (an internal
    /// function, or eval'd code). Absence is not evidence — the path rules treat an
    /// unknown file as unknown rather than as a dependency.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        if self.origin.file.is_empty() {
            None
        } else {
            Some(&self.origin.file)
        }
    }
}

/// Per-process, monotonic name interner. THE reason Tier 1 is affordable.
///
/// Keyed on the runtime's own function handle — the same key Zend caches the
/// observer-factory verdict on — so the expensive `Class::method` formatting and the
/// frame's owned copy happen once per FUNCTION per PROCESS instead of once per CALL.
///
/// Never shrinks and never rekeys. A `zend_function` lives as long as the process, and
/// reusing a freed handle would silently merge two functions into one identity, which
/// is the worst possible failure mode: the numbers would be wrong and nothing would
/// look broken.
#[derive(Default)]
pub struct NameInterner {
    by_handle: BTreeMap<usize, FunctionId>,
    /// Only ever populated by [`NameInterner::intern_named`] — the fallback used when
    /// no stable handle exists. Kept out of the handle path so the common case pays
    /// one probe, not two.
    by_name: BTreeMap<Rc<str>, FunctionId>,
    names: Vec<Rc<str>>,
    origins: Vec<FunctionOrigin>,
    internal: Vec<bool>,
}

impl NameInterner {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            by_handle: BTreeMap::new(),
            by_name: BTreeMap::new(),
            names: Vec::new(),
            origins: Vec::new(),
            internal: Vec::new(),
        }
    }

    /// Resolve `handle` to an interned function, calling `resolve` ONLY on a miss.
    ///
    /// The closure is where the `format!` and the origin reads live, so a hit costs one
    /// map probe and three refcount bumps.
    pub fn intern<F>(&mut self, handle: usize, resolve: F) -> InternedFunction
    where
        F: FnOnce() -> FunctionFacts,
    {
        if let Some(&id) = self.by_handle.get(&handle) {
            if let Some(interned) = self.get(id) {
                return interned;
            }
        }
        let interned = self.insert(resolve());
        self.by_handle.insert(handle, interned.id);
        interned
    }

    /// Fallback for a call whose function handle is unavailable: intern by NAME.
    ///
    /// Costs a string comparison per call rather than per function, which is the price
    /// of not having a key — but it never hands out a wrong id, and it keeps the drain
    /// path total rather than silently missing rows.
    pub fn intern_named(&mut self, facts: FunctionFacts) -> InternedFunction {
        if let Some(&id) = self.by_name.get(facts.name.as_str()) {
            if let Some(interned) = self.get(id) {
                return interned;
            }
        }
        let interned = self.insert(facts);
        self.by_name.insert(interned.name.clone(), interned.id);
        interned
    }

    fn insert(&mut self, facts: FunctionFacts) -> InternedFunction {
        let id = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        let name: Rc<str> = Rc::from(facts.name.as_str());
        self.names.push(name.clone());
        self.origins.push(facts.origin.clone());
        self.internal.push(facts.internal);
        InternedFunction {
            id,
            name,
            origin: facts.origin,
            internal: facts.internal,
        }
    }

    #[must_use]
    pub fn get(&self, id: FunctionId) -> Option<InternedFunction> {
        let index = id as usize;
        Some(InternedFunction {
            id,
            name: self.names.get(index)?.clone(),
            origin: self.origins.get(index)?.clone(),
            internal: *self.internal.get(index)?,
        })
    }

    #[must_use]
    pub fn lookup(&self, handle: usize) -> Option<FunctionId> {
        self.by_handle.get(&handle).copied()
    }

    /// Canonical identity. Exactly the string the sampler writes to
    /// `SampleFrame::function` and the desktop's flame path carries as a segment.
    #[must_use]
    pub fn name(&self, id: FunctionId) -> Option<&Rc<str>> {
        self.names.get(id as usize)
    }

    #[must_use]
    pub fn origin(&self, id: FunctionId) -> Option<&FunctionOrigin> {
        self.origins.get(id as usize)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Resolved once per request from `settings::*`, mirroring [`crate::sampler::SamplerConfig::resolve`].
///
/// Every read goes through `settings` rather than `std::env::var`, so INI
/// (`chronos.profile_deterministic`) and a `.chronos` file work too. All nine names are
/// registered in [`crate::settings::SETTING_NAMES`] — without that registration
/// `settings::get` still reads process env, so the setting APPEARS to work while INI
/// and `.chronos` silently do not exist. That is the quiet failure mode this comment is
/// here to prevent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeterministicConfig {
    /// Tier 1 kill switch. `CHRONOS_PHP_PROFILE_DETERMINISTIC`, default TRUE — the only
    /// signal flag in the collector that defaults on, for the O(distinct functions)
    /// reason in the module docs.
    pub aggregates: bool,
    /// `CHRONOS_PHP_PROFILE_DETERMINISTIC_MAX_FUNCTIONS`, default 4096. The number that
    /// makes the O(distinct functions) claim concrete; overflow is REPORTED in
    /// `truncation.functionsSeen`, never silently dropped.
    pub max_functions: usize,
    /// Tier 2. `CHRONOS_PHP_PROFILE_EDGES`, default FALSE. O(distinct edges) is roughly
    /// an order of magnitude above O(distinct functions), and the caller/callee panel is
    /// a drill-down rather than a landing page.
    pub edges: bool,
    /// `CHRONOS_PHP_PROFILE_EDGES_MAX`, default 8192.
    pub max_edges: usize,
    /// Tier 3. `CHRONOS_PHP_PROFILE_ARGS`, default FALSE. Even when true the buffer
    /// refuses to record until [`DeterministicBuffer::arm_arguments`] is called, which
    /// the request path does only for a forced profile (`X-Chronos-Profile: <token>`)
    /// or an armed DST recording. Two independent gates, deliberately.
    pub arguments: bool,
    /// `CHRONOS_PHP_PROFILE_ARGS_MAX_ARGS`, default 8.
    pub max_arguments: usize,
    /// `CHRONOS_PHP_PROFILE_ARGS_MAX_ARG_BYTES`, default 256. Clipped on a UTF-8
    /// boundary and flagged `truncated`, never silently shortened.
    pub max_argument_bytes: usize,
    /// `CHRONOS_PHP_PROFILE_ARGS_MAX_TOTAL_BYTES`, default 4096.
    pub max_argument_total_bytes: usize,
    /// `CHRONOS_PHP_PROFILE_ARGS_MAX_INVOCATIONS`, default 4, so a hot allowlisted
    /// function cannot turn a forced profile into a flood.
    pub max_argument_invocations: usize,
}

impl DeterministicConfig {
    pub const DEFAULT_MAX_FUNCTIONS: usize = 4_096;
    pub const DEFAULT_MAX_EDGES: usize = 8_192;
    pub const DEFAULT_MAX_ARGUMENTS: usize = 8;
    pub const DEFAULT_MAX_ARGUMENT_BYTES: usize = 256;
    pub const DEFAULT_MAX_ARGUMENT_TOTAL_BYTES: usize = 4_096;
    pub const DEFAULT_MAX_ARGUMENT_INVOCATIONS: usize = 4;

    /// Everything off. The between-requests posture, and what a disabled collector
    /// resolves to — a buffer holding this config records nothing at all.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            aggregates: false,
            max_functions: 0,
            edges: false,
            max_edges: 0,
            arguments: false,
            max_arguments: 0,
            max_argument_bytes: 0,
            max_argument_total_bytes: 0,
            max_argument_invocations: 0,
        }
    }

    /// Read every knob through `settings` and clamp it into a band the request can
    /// afford. Unlike `SamplerConfig::resolve` this never returns `None`: Tier 1 is
    /// always-on, so "off" is a value of `aggregates`, not an absent config.
    #[must_use]
    pub fn resolve() -> Self {
        use crate::settings;
        Self {
            aggregates: settings::flag("CHRONOS_PHP_PROFILE_DETERMINISTIC", true),
            max_functions: settings::u64_value(
                "CHRONOS_PHP_PROFILE_DETERMINISTIC_MAX_FUNCTIONS",
                Self::DEFAULT_MAX_FUNCTIONS as u64,
            ) as usize,
            edges: settings::flag("CHRONOS_PHP_PROFILE_EDGES", false),
            max_edges: settings::u64_value(
                "CHRONOS_PHP_PROFILE_EDGES_MAX",
                Self::DEFAULT_MAX_EDGES as u64,
            ) as usize,
            arguments: settings::flag("CHRONOS_PHP_PROFILE_ARGS", false),
            max_arguments: settings::u64_value(
                "CHRONOS_PHP_PROFILE_ARGS_MAX_ARGS",
                Self::DEFAULT_MAX_ARGUMENTS as u64,
            ) as usize,
            max_argument_bytes: settings::u64_value(
                "CHRONOS_PHP_PROFILE_ARGS_MAX_ARG_BYTES",
                Self::DEFAULT_MAX_ARGUMENT_BYTES as u64,
            ) as usize,
            max_argument_total_bytes: settings::u64_value(
                "CHRONOS_PHP_PROFILE_ARGS_MAX_TOTAL_BYTES",
                Self::DEFAULT_MAX_ARGUMENT_TOTAL_BYTES as u64,
            ) as usize,
            max_argument_invocations: settings::u64_value(
                "CHRONOS_PHP_PROFILE_ARGS_MAX_INVOCATIONS",
                Self::DEFAULT_MAX_ARGUMENT_INVOCATIONS as u64,
            ) as usize,
        }
        .clamped()
    }

    /// Bound every cap. Separated from [`Self::resolve`] so the bounds are unit-testable
    /// without mutating process environment, which races every other test in the crate.
    ///
    /// A written `0` for a cap means "retain nothing", which would make Tier 1 report an
    /// empty window while claiming coverage — so caps floor at 1 and only the tier flags
    /// turn a tier off. The argument caps are the exception: `0` there is a coherent
    /// "types only, no values" posture and is preserved.
    #[must_use]
    pub const fn clamped(self) -> Self {
        Self {
            aggregates: self.aggregates,
            max_functions: clamp_usize(self.max_functions, 1, 262_144),
            edges: self.edges,
            max_edges: clamp_usize(self.max_edges, 1, 1_048_576),
            arguments: self.arguments,
            max_arguments: clamp_usize(self.max_arguments, 0, 64),
            max_argument_bytes: clamp_usize(self.max_argument_bytes, 0, 8_192),
            max_argument_total_bytes: clamp_usize(self.max_argument_total_bytes, 0, 262_144),
            max_argument_invocations: clamp_usize(self.max_argument_invocations, 0, 256),
        }
    }
}

const fn clamp_usize(value: usize, low: usize, high: usize) -> usize {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

impl Default for DeterministicConfig {
    fn default() -> Self {
        Self {
            aggregates: true,
            max_functions: Self::DEFAULT_MAX_FUNCTIONS,
            edges: false,
            max_edges: Self::DEFAULT_MAX_EDGES,
            arguments: false,
            max_arguments: Self::DEFAULT_MAX_ARGUMENTS,
            max_argument_bytes: Self::DEFAULT_MAX_ARGUMENT_BYTES,
            max_argument_total_bytes: Self::DEFAULT_MAX_ARGUMENT_TOTAL_BYTES,
            max_argument_invocations: Self::DEFAULT_MAX_ARGUMENT_INVOCATIONS,
        }
    }
}

// ---------------------------------------------------------------------------
// Tier 3 argument capture
// ---------------------------------------------------------------------------

/// Scalar-only argument vocabulary. The three composite members carry a TYPE and NEVER
/// a value — "argument 3 was an array" is evidence and leaks nothing, while the array
/// itself is unbounded application data and is refused (ADR 0017, ADR 0029 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentType {
    Null,
    Bool,
    Int,
    Float,
    Str,
    Object,
    Array,
    Resource,
}

impl ArgumentType {
    /// The `DETERMINISTIC_ARGUMENT_TYPE_*` proto/JSON spelling.
    #[must_use]
    pub const fn proto_name(self) -> &'static str {
        match self {
            Self::Null => "DETERMINISTIC_ARGUMENT_TYPE_NULL",
            Self::Bool => "DETERMINISTIC_ARGUMENT_TYPE_BOOL",
            Self::Int => "DETERMINISTIC_ARGUMENT_TYPE_INT",
            Self::Float => "DETERMINISTIC_ARGUMENT_TYPE_FLOAT",
            Self::Str => "DETERMINISTIC_ARGUMENT_TYPE_STRING",
            Self::Object => "DETERMINISTIC_ARGUMENT_TYPE_OBJECT",
            Self::Array => "DETERMINISTIC_ARGUMENT_TYPE_ARRAY",
            Self::Resource => "DETERMINISTIC_ARGUMENT_TYPE_RESOURCE",
        }
    }

    /// Whether a value may be carried at all. Composites: never.
    #[must_use]
    pub const fn carries_value(self) -> bool {
        matches!(
            self,
            Self::Null | Self::Bool | Self::Int | Self::Float | Self::Str
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedArgument {
    pub position: u32,
    /// Declared parameter name when the runtime exposes one, else empty.
    pub name: String,
    pub argument_type: ArgumentType,
    /// EMPTY for every composite type, and for a redacted or absent scalar. Empty is
    /// never "the empty string" — `argument_type` and the flags say which it is.
    pub value: String,
    pub redacted: bool,
    pub truncated: bool,
}

/// Build one captured argument, applying the value rules in one place.
///
/// `redact` is decided by the CALLER against the collector's existing redaction pattern
/// list (`http_capture`), so this module stays free of configuration and the mask
/// applied to a `$apiToken` parameter is the same mask applied to an `Authorization`
/// header. `raw` is `None` whenever the runtime had no scalar to offer, which includes
/// PHP `null` — a NULL-typed argument carries a type and no value, exactly like a
/// composite, and the type field is what tells them apart.
#[must_use]
pub fn capture_argument(
    position: u32,
    name: String,
    argument_type: ArgumentType,
    raw: Option<String>,
    redact: bool,
    max_bytes: usize,
) -> CapturedArgument {
    // Composites are refused BEFORE redaction so no code path can be reordered into
    // serialising an object's contents.
    if !argument_type.carries_value() {
        return CapturedArgument {
            position,
            name,
            argument_type,
            value: String::new(),
            redacted: false,
            truncated: false,
        };
    }
    if redact {
        return CapturedArgument {
            position,
            name,
            argument_type,
            value: crate::http_capture::MASK.to_owned(),
            redacted: true,
            truncated: false,
        };
    }
    let Some(raw) = raw else {
        return CapturedArgument {
            position,
            name,
            argument_type,
            value: String::new(),
            redacted: false,
            truncated: false,
        };
    };
    let (value, truncated) = clip_utf8(raw, max_bytes);
    CapturedArgument {
        position,
        name,
        argument_type,
        value,
        redacted: false,
        truncated,
    }
}

/// Clip to `max_bytes` on a UTF-8 boundary, reporting whether anything was removed.
///
/// Byte-offset slicing would panic mid-codepoint, and `panic = "abort"` is set for the
/// release profile — a multi-byte argument value would take the whole worker down.
#[must_use]
fn clip_utf8(mut value: String, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgumentSample {
    /// 1-based invocation index within the window. Counts TRUE invocations, so a gap
    /// between retained samples says the invocation cap refused the ones between.
    pub invocation: u64,
    pub arguments: Vec<CapturedArgument>,
    pub arguments_dropped: u32,
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// One function's totals for the window, plus the transient recursion depth the banking
/// rule needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FunctionTotals {
    pub call_count: u64,
    /// Banked on the OUTERMOST frame only (ADR 0029 §3).
    pub inclusive_nanoseconds: u64,
    /// Banked at EVERY depth.
    pub exclusive_nanoseconds: u64,
    /// Deepest simultaneous nesting seen. `1` for an ordinary function; `> 1`
    /// invalidates `inclusive - exclusive` as child time.
    pub max_recursion_depth: u32,
    /// Frames of this function currently on the stack. Transient; never emitted.
    pub live_depth: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeTotals {
    pub call_count: u64,
    pub inclusive_nanoseconds: u64,
}

/// Per-frame state the observer carries alongside its `CallFrame`.
///
/// `started_at_nanos` is what fills the timing that `CallFrame::observe_only` cannot:
/// `start_hrtime` stays `0` on an `ObserveOnly` frame deliberately, because that field
/// feeds `on_end`'s span-duration and min-duration logic and an `ObserveOnly` frame
/// must never become a span. Two fields, two purposes, no possibility of confusion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameTiming {
    pub function: FunctionId,
    pub started_at_nanos: u64,
    /// Inclusive time of everything this frame called, accumulated by the children as
    /// they leave.
    pub child_nanoseconds: u64,
    /// Whether this frame was counted at all. False on a frame pushed while the buffer
    /// was disarmed, so its `on_leave` cannot bank time against a `call_count` that was
    /// never incremented.
    pub counted: bool,
}

/// In-band truncation. A capped set always says it was capped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Truncation {
    pub functions_seen: u64,
    pub functions_kept: u64,
    pub edges_seen: u64,
    pub edges_kept: u64,
    pub argument_bytes_dropped: u64,
}

impl Truncation {
    /// Whether the function set was capped. Reported so a reader can tell a short list
    /// from a complete one — a top-N that does not admit it is a top-N is a lie.
    #[must_use]
    pub const fn truncated_functions(self) -> bool {
        self.functions_seen > self.functions_kept
    }

    #[must_use]
    pub const fn truncated_edges(self) -> bool {
        self.edges_seen > self.edges_kept
    }

    #[must_use]
    pub const fn truncated_arguments(self) -> bool {
        self.argument_bytes_dropped > 0
    }

    #[must_use]
    pub const fn truncated(self) -> bool {
        self.truncated_functions() || self.truncated_edges() || self.truncated_arguments()
    }
}

/// Which tiers this window actually covered. DECLARED, never inferred: an absent tier
/// means NOT COLLECTED and must never render as zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TierCoverage {
    pub aggregates: bool,
    pub edges: bool,
    pub arguments: bool,
}

/// One request's deterministic aggregate.
///
/// Memory is O(distinct functions) + O(distinct edges), NOT O(calls).
#[derive(Default)]
pub struct DeterministicBuffer {
    config: DeterministicConfig,
    functions: BTreeMap<FunctionId, FunctionTotals>,
    edges: BTreeMap<(FunctionId, FunctionId), EdgeTotals>,
    arguments: BTreeMap<FunctionId, Vec<ArgumentSample>>,
    invocations: BTreeMap<FunctionId, u64>,
    truncation: Truncation,
    argument_bytes_used: usize,
    /// Tier 3 gate: set only for a forced profile or an armed DST recording.
    arguments_armed: bool,
}

impl DeterministicBuffer {
    #[must_use]
    pub const fn new(config: DeterministicConfig) -> Self {
        Self {
            config,
            functions: BTreeMap::new(),
            edges: BTreeMap::new(),
            arguments: BTreeMap::new(),
            invocations: BTreeMap::new(),
            truncation: Truncation {
                functions_seen: 0,
                functions_kept: 0,
                edges_seen: 0,
                edges_kept: 0,
                argument_bytes_dropped: 0,
            },
            argument_bytes_used: 0,
            arguments_armed: false,
        }
    }

    /// RINIT. Clears the window and re-reads nothing: config is resolved by the caller.
    pub fn reset(&mut self, config: DeterministicConfig) {
        self.config = config;
        self.functions.clear();
        self.edges.clear();
        self.arguments.clear();
        self.invocations.clear();
        self.truncation = Truncation::default();
        self.argument_bytes_used = 0;
        self.arguments_armed = false;
    }

    /// Tier 3 gate. Called from the request path ONLY when the forced-profile directive
    /// matched `profile_token` or DST is armed.
    pub fn arm_arguments(&mut self) {
        self.arguments_armed = true;
    }

    #[must_use]
    pub const fn config(&self) -> DeterministicConfig {
        self.config
    }

    #[must_use]
    pub const fn aggregates_active(&self) -> bool {
        self.config.aggregates
    }

    #[must_use]
    pub const fn arguments_active(&self) -> bool {
        self.config.arguments && self.arguments_armed
    }

    #[must_use]
    pub const fn coverage(&self) -> TierCoverage {
        TierCoverage {
            aggregates: self.config.aggregates,
            edges: self.config.aggregates && self.config.edges,
            arguments: self.arguments_active(),
        }
    }

    /// Hot path, BEGIN side. Piggybacks on the existing `CallFrame` push — it must never
    /// push a parallel stack, or it desyncs on the requests where the `ObserveOnly` push
    /// is skipped.
    ///
    /// Returns the per-frame state the caller stores on its own `CallFrame`.
    pub fn on_enter(&mut self, function: FunctionId, now_nanos: u64) -> FrameTiming {
        let mut counted = false;
        if self.config.aggregates {
            let capped = self.functions.len() >= self.config.max_functions
                && !self.functions.contains_key(&function);
            if capped {
                // Reported, never silently dropped: a capped set that does not say it
                // was capped is a number that lies.
                self.truncation.functions_seen = self.truncation.functions_seen.saturating_add(1);
            } else {
                let totals = self.functions.entry(function).or_default();
                totals.call_count = totals.call_count.saturating_add(1);
                totals.live_depth = totals.live_depth.saturating_add(1);
                totals.max_recursion_depth = totals.max_recursion_depth.max(totals.live_depth);
                counted = true;
            }
        }
        FrameTiming {
            function,
            started_at_nanos: now_nanos,
            child_nanoseconds: 0,
            counted,
        }
    }

    /// Hot path, END side. Returns this frame's INCLUSIVE duration, which the caller adds
    /// to the parent frame's `child_nanoseconds`.
    ///
    /// Returning the duration rather than reaching for the parent keeps the borrow
    /// trivial and keeps the frame stack the observer's, not this module's.
    pub fn on_leave(
        &mut self,
        frame: &FrameTiming,
        caller: Option<FunctionId>,
        now_nanos: u64,
    ) -> u64 {
        let duration = now_nanos.saturating_sub(frame.started_at_nanos);
        if !self.config.aggregates || !frame.counted {
            return duration;
        }
        let exclusive = duration.saturating_sub(frame.child_nanoseconds);
        if let Some(totals) = self.functions.get_mut(&frame.function) {
            // Exclusive at every depth: self time is not double counted by recursion.
            totals.exclusive_nanoseconds = totals.exclusive_nanoseconds.saturating_add(exclusive);
            totals.live_depth = totals.live_depth.saturating_sub(1);
            if totals.live_depth == 0 {
                // Inclusive on the outermost frame only, or a depth-N recursion banks
                // its inclusive time N times.
                totals.inclusive_nanoseconds =
                    totals.inclusive_nanoseconds.saturating_add(duration);
            }
        }
        if self.config.edges {
            if let Some(caller) = caller {
                self.record_edge(caller, frame.function, duration);
            }
        }
        duration
    }

    fn record_edge(&mut self, caller: FunctionId, callee: FunctionId, duration: u64) {
        let key = (caller, callee);
        if self.edges.len() >= self.config.max_edges && !self.edges.contains_key(&key) {
            self.truncation.edges_seen = self.truncation.edges_seen.saturating_add(1);
            return;
        }
        let edge = self.edges.entry(key).or_default();
        edge.call_count = edge.call_count.saturating_add(1);
        edge.inclusive_nanoseconds = edge.inclusive_nanoseconds.saturating_add(duration);
    }

    /// Tier 3. `arguments` must ALREADY be redacted, typed and clipped by the caller;
    /// this enforces only the count, invocation and byte budgets. Returns false when the
    /// sample was refused, which the caller must not treat as an error.
    ///
    /// The caller must have checked the instrumentation manifest first — Tier 3 is
    /// allowlisted, and this module deliberately does not know the allowlist so it
    /// cannot be talked into bypassing it.
    pub fn record_arguments(
        &mut self,
        function: FunctionId,
        arguments: Vec<CapturedArgument>,
        arguments_dropped: u32,
    ) -> bool {
        if !self.arguments_active() {
            return false;
        }
        let invocation = {
            let counter = self.invocations.entry(function).or_default();
            *counter = counter.saturating_add(1);
            *counter
        };
        let taken = self.arguments.entry(function).or_default();
        if taken.len() >= self.config.max_argument_invocations {
            return false;
        }
        let mut arguments = arguments;
        let dropped_by_count = arguments.len().saturating_sub(self.config.max_arguments);
        arguments.truncate(self.config.max_arguments);
        let bytes: usize = arguments.iter().map(|argument| argument.value.len()).sum();
        if self.argument_bytes_used.saturating_add(bytes) > self.config.max_argument_total_bytes {
            self.truncation.argument_bytes_dropped = self
                .truncation
                .argument_bytes_dropped
                .saturating_add(bytes as u64);
            return false;
        }
        self.argument_bytes_used = self.argument_bytes_used.saturating_add(bytes);
        taken.push(ArgumentSample {
            invocation,
            arguments,
            arguments_dropped: arguments_dropped
                .saturating_add(u32::try_from(dropped_by_count).unwrap_or(u32::MAX)),
        });
        true
    }

    /// RSHUTDOWN. Consumes the window into the rows the spool writer serialises.
    ///
    /// Rows are ordered by exclusive time descending so a cap at the spool layer keeps
    /// the interesting end, and ties break on the canonical identity so the document is
    /// byte-stable for a given request — content-addressed spool filenames depend on it.
    #[must_use]
    pub fn drain(&mut self, interner: &NameInterner) -> DeterministicWindow {
        let coverage = self.coverage();
        let mut rows: Vec<FunctionRow> = self
            .functions
            .iter()
            .filter_map(|(&id, totals)| {
                let name = interner.name(id)?.clone();
                let origin = interner.origin(id).cloned().unwrap_or_default();
                let mut callees: Vec<EdgeRow> = if coverage.edges {
                    self.edges
                        .iter()
                        .filter(|((caller, _), _)| *caller == id)
                        .filter_map(|((_, callee), edge)| {
                            Some(EdgeRow {
                                callee: interner.name(*callee)?.clone(),
                                call_count: edge.call_count,
                                inclusive_nanoseconds: edge.inclusive_nanoseconds,
                            })
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                callees.sort_by(|left, right| {
                    right
                        .inclusive_nanoseconds
                        .cmp(&left.inclusive_nanoseconds)
                        .then_with(|| left.callee.cmp(&right.callee))
                });
                Some(FunctionRow {
                    function: name,
                    module: origin.module,
                    file: origin.file,
                    line: origin.line,
                    call_count: totals.call_count,
                    inclusive_nanoseconds: totals.inclusive_nanoseconds,
                    exclusive_nanoseconds: totals.exclusive_nanoseconds,
                    max_recursion_depth: totals.max_recursion_depth.max(1),
                    callees,
                    argument_samples: self.arguments.get(&id).cloned().unwrap_or_default(),
                })
            })
            .collect();
        rows.sort_by(|left, right| {
            right
                .exclusive_nanoseconds
                .cmp(&left.exclusive_nanoseconds)
                .then_with(|| left.function.cmp(&right.function))
        });
        let mut truncation = self.truncation;
        truncation.functions_kept = rows.len() as u64;
        truncation.functions_seen = truncation.functions_seen.saturating_add(rows.len() as u64);
        truncation.edges_kept = self.edges.len() as u64;
        truncation.edges_seen = truncation
            .edges_seen
            .saturating_add(self.edges.len() as u64);
        DeterministicWindow {
            functions: rows,
            coverage,
            truncation,
        }
    }
}

/// One caller→callee edge as emitted. The caller is the enclosing [`FunctionRow`], so
/// direction is unambiguous and the identity is not repeated per edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeRow {
    pub callee: Rc<str>,
    pub call_count: u64,
    pub inclusive_nanoseconds: u64,
}

/// One function's row as emitted. `function` is the canonical identity and the join key;
/// `module`/`file`/`line` are descriptive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionRow {
    pub function: Rc<str>,
    pub module: Rc<str>,
    pub file: Rc<str>,
    pub line: u32,
    pub call_count: u64,
    pub inclusive_nanoseconds: u64,
    pub exclusive_nanoseconds: u64,
    pub max_recursion_depth: u32,
    pub callees: Vec<EdgeRow>,
    pub argument_samples: Vec<ArgumentSample>,
}

/// What the spool writer serialises into `chronos.profiling.deterministic-batch.v1`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeterministicWindow {
    pub functions: Vec<FunctionRow>,
    pub coverage: TierCoverage,
    pub truncation: Truncation,
}

impl DeterministicWindow {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Request-local state
// ---------------------------------------------------------------------------

thread_local! {
    /// Per-PROCESS in spirit, per-thread in fact: PHP-FPM is one request per worker and
    /// the collector is `thread_local!` throughout, so a thread-local interner is a
    /// process-local one. A ZTS build would key this by thread-safe resource id, which
    /// is what a thread_local already does.
    ///
    /// NOT reset between requests. A `zend_function` outlives the request, so throwing
    /// the interner away at RSHUTDOWN would reintroduce the per-call `format!` on the
    /// first request after every reset — which is the entire cost this exists to remove.
    static INTERNER: RefCell<NameInterner> = const { RefCell::new(NameInterner::new()) };

    /// Reset at RINIT, drained at RSHUTDOWN, exactly like `sampler::REQUEST_SAMPLES`.
    /// Holds [`DeterministicConfig::off`] between requests so a call observed outside a
    /// request records nothing.
    static BUFFER: RefCell<DeterministicBuffer> =
        const { RefCell::new(DeterministicBuffer::new(DeterministicConfig::off())) };

    /// Tier 3's two-flag verdict, precomputed.
    ///
    /// A `Cell<bool>` mirroring [`DeterministicBuffer::arguments_active`] rather than a
    /// borrow of it, because this is read once per OBSERVED CALL on every request —
    /// including the overwhelming majority where Tier 3 is off and the answer is a
    /// foregone no. The buffer stays the authority; this is the fast path's copy of its
    /// answer, and both are written by the same two functions.
    static ARGUMENTS_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RINIT: arm the window for this request. `reset` clears the arming, so the fast gate
/// is re-derived here rather than left holding the previous request's answer.
pub fn reset_request(config: DeterministicConfig) {
    let active = BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.reset(config);
        buffer.arguments_active()
    });
    ARGUMENTS_ACTIVE.with(|gate| gate.set(active));
}

/// Arm Tier 3 for this request. Called only for a forced profile or an armed DST
/// recording — `CHRONOS_PHP_PROFILE_ARGS` alone is not enough, deliberately.
pub fn arm_arguments() {
    let active = BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.arm_arguments();
        buffer.arguments_active()
    });
    ARGUMENTS_ACTIVE.with(|gate| gate.set(active));
}

#[must_use]
pub fn aggregates_active() -> bool {
    BUFFER.with(|buffer| buffer.borrow().aggregates_active())
}

/// The observed-call fast path's Tier 3 gate. See [`ARGUMENTS_ACTIVE`].
#[must_use]
pub fn arguments_active() -> bool {
    ARGUMENTS_ACTIVE.with(std::cell::Cell::get)
}

#[must_use]
pub fn config() -> DeterministicConfig {
    BUFFER.with(|buffer| buffer.borrow().config())
}

/// Intern by the runtime's function handle. See [`NameInterner::intern`].
pub fn intern<F>(handle: usize, resolve: F) -> InternedFunction
where
    F: FnOnce() -> FunctionFacts,
{
    INTERNER.with(|interner| interner.borrow_mut().intern(handle, resolve))
}

/// Intern by name, for a call whose function handle is unavailable.
pub fn intern_named(facts: FunctionFacts) -> InternedFunction {
    INTERNER.with(|interner| interner.borrow_mut().intern_named(facts))
}

/// Hot path, BEGIN side. See [`DeterministicBuffer::on_enter`].
pub fn on_enter(function: FunctionId, now_nanos: u64) -> FrameTiming {
    BUFFER.with(|buffer| buffer.borrow_mut().on_enter(function, now_nanos))
}

/// Hot path, END side. See [`DeterministicBuffer::on_leave`].
pub fn on_leave(frame: &FrameTiming, caller: Option<FunctionId>, now_nanos: u64) -> u64 {
    BUFFER.with(|buffer| buffer.borrow_mut().on_leave(frame, caller, now_nanos))
}

/// Tier 3. See [`DeterministicBuffer::record_arguments`].
pub fn record_arguments(
    function: FunctionId,
    arguments: Vec<CapturedArgument>,
    arguments_dropped: u32,
) -> bool {
    BUFFER.with(|buffer| {
        buffer
            .borrow_mut()
            .record_arguments(function, arguments, arguments_dropped)
    })
}

/// RSHUTDOWN: consume the window. The interner is deliberately NOT drained with it.
#[must_use]
pub fn drain_window() -> DeterministicWindow {
    INTERNER.with(|interner| {
        let interner = interner.borrow();
        BUFFER.with(|buffer| buffer.borrow_mut().drain(&interner))
    })
}

/// Distinct functions interned so far in this worker. Diagnostics only.
#[must_use]
pub fn interned_count() -> usize {
    INTERNER.with(|interner| interner.borrow().len())
}

// ---------------------------------------------------------------------------
// Executable specification. These tests ARE the recursion rule.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: &str) -> FunctionFacts {
        FunctionFacts {
            name: name.to_owned(),
            origin: FunctionOrigin::default(),
            internal: false,
        }
    }

    fn interner_with(names: &[&str]) -> (NameInterner, Vec<FunctionId>) {
        let mut interner = NameInterner::new();
        let ids = names
            .iter()
            .enumerate()
            .map(|(index, name)| interner.intern(index + 1, || facts(name)).id)
            .collect();
        (interner, ids)
    }

    fn edge_config() -> DeterministicConfig {
        DeterministicConfig {
            edges: true,
            ..DeterministicConfig::default()
        }
    }

    // --- Interning ---------------------------------------------------------

    #[test]
    fn interning_resolves_once_per_function() {
        let mut interner = NameInterner::new();
        let mut calls = 0;
        for _ in 0..1_000 {
            interner.intern(0x1000, || {
                calls += 1;
                facts("App\\Orders\\Repository::find")
            });
        }
        assert_eq!(
            calls, 1,
            "the format! must run once per function, not per call"
        );
        assert_eq!(interner.len(), 1);
    }

    #[test]
    fn interning_returns_a_stable_id_and_resolves_the_same_name() {
        let mut interner = NameInterner::new();
        let first = interner.intern(0x2000, || facts("App\\Orders\\Service::purchaseLabel"));
        let second = interner.intern(0x2000, || facts("SHOULD NEVER BE CALLED"));
        assert_eq!(first.id, second.id);
        assert_eq!(&*second.name, "App\\Orders\\Service::purchaseLabel");
        assert_eq!(interner.lookup(0x2000), Some(first.id));
        assert_eq!(
            interner.name(first.id).map(|name| name.to_string()),
            Some("App\\Orders\\Service::purchaseLabel".to_owned())
        );
    }

    #[test]
    fn distinct_handles_are_distinct_identities() {
        let (interner, ids) = interner_with(&["a", "b"]);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn identity_is_never_case_folded() {
        // PHP call sites are case-insensitive but the Zend struct stores the DECLARED
        // spelling, and the join key is that spelling. Folding here would merge two
        // identities the runtime kept apart.
        let mut interner = NameInterner::new();
        let lower = interner.intern(1, || facts("app\\orders\\repository::find"));
        let declared = interner.intern(2, || facts("App\\Orders\\Repository::find"));
        assert_ne!(lower.id, declared.id);
    }

    #[test]
    fn the_name_fallback_interns_without_a_handle() {
        // The str-based path: no stable key, so identity comes from the name itself.
        let mut interner = NameInterner::new();
        let first = interner.intern_named(facts("App\\Support\\slugify"));
        let second = interner.intern_named(facts("App\\Support\\slugify"));
        assert_eq!(first.id, second.id);
        assert_eq!(interner.len(), 1);
        // And it does not collide with the handle-keyed map.
        let handled = interner.intern(0x3000, || facts("App\\Support\\slugify"));
        assert_ne!(handled.id, first.id);
    }

    #[test]
    fn the_origin_travels_with_the_identity_but_is_not_part_of_it() {
        let mut interner = NameInterner::new();
        let interned = interner.intern(1, || FunctionFacts {
            name: "App\\Orders\\Repository::find".to_owned(),
            origin: FunctionOrigin {
                module: Rc::from("php"),
                file: Rc::from("/srv/app/src/Orders/Repository.php"),
                line: 41,
            },
            internal: false,
        });
        assert_eq!(interned.file(), Some("/srv/app/src/Orders/Repository.php"));
        assert_eq!(interner.origin(interned.id).map(|o| o.line), Some(41));
        // An unknown file reads as absent, never as an empty path.
        let unknown = interner.intern(2, || facts("strlen"));
        assert_eq!(unknown.file(), None);
    }

    // --- Recursion ---------------------------------------------------------

    #[test]
    fn direct_recursion_banks_inclusive_once_and_exclusive_every_depth() {
        let (interner, ids) = interner_with(&["walk"]);
        let walk = ids[0];
        let mut buffer = DeterministicBuffer::new(DeterministicConfig::default());
        // walk() at t=0 calls walk() at t=10 which returns at t=30; outer at t=40.
        let mut outer = buffer.on_enter(walk, 0);
        let inner = buffer.on_enter(walk, 10);
        let inner_duration = buffer.on_leave(&inner, Some(walk), 30);
        outer.child_nanoseconds += inner_duration;
        buffer.on_leave(&outer, None, 40);

        let window = buffer.drain(&interner);
        let row = &window.functions[0];
        assert_eq!(row.call_count, 2);
        // Outermost frame only: 40, not 40 + 20.
        assert_eq!(row.inclusive_nanoseconds, 40);
        // Every depth: inner 20 + outer (40 - 20) = 40.
        assert_eq!(row.exclusive_nanoseconds, 40);
        assert_eq!(row.max_recursion_depth, 2);
    }

    #[test]
    fn deep_direct_recursion_does_not_multiply_inclusive_time_by_depth() {
        // The failure this rule exists to prevent: a depth-8 tree walk reporting eight
        // times the wall time of the request that contained it.
        let (interner, ids) = interner_with(&["walk"]);
        let walk = ids[0];
        let mut buffer = DeterministicBuffer::new(DeterministicConfig::default());
        const DEPTH: u64 = 8;
        let mut frames: Vec<FrameTiming> = Vec::new();
        for depth in 0..DEPTH {
            frames.push(buffer.on_enter(walk, depth));
        }
        // Unwind innermost first; each frame lives 1ns longer than the one inside it.
        for depth in (0..DEPTH).rev() {
            let frame = frames.pop().expect("frame");
            let duration = buffer.on_leave(&frame, Some(walk), 2 * DEPTH - depth);
            if let Some(parent) = frames.last_mut() {
                parent.child_nanoseconds += duration;
            }
        }
        let window = buffer.drain(&interner);
        let row = &window.functions[0];
        assert_eq!(row.call_count, DEPTH);
        assert_eq!(row.max_recursion_depth, DEPTH as u32);
        // The outermost frame ran 0 -> 16.
        assert_eq!(row.inclusive_nanoseconds, 16);
        // Exclusive banked at every depth sums to the outermost frame's own span,
        // because every nanosecond of it was spent inside SOME frame of `walk`.
        assert_eq!(row.exclusive_nanoseconds, 16);
        // And the naive subtraction a reader must NOT make.
        assert_eq!(
            row.inclusive_nanoseconds - row.exclusive_nanoseconds,
            0,
            "inclusive - exclusive is meaningless above depth 1; the depth field says so"
        );
    }

    #[test]
    fn mutual_recursion_banks_each_function_on_its_own_outermost_frame() {
        // ping(0..60) -> pong(10..50) -> ping(20..40) -> pong(25..35).
        // Neither function may have its inclusive time counted twice, and the depth of
        // each is tracked independently of the other's.
        let (interner, ids) = interner_with(&["ping", "pong"]);
        let (ping, pong) = (ids[0], ids[1]);
        let mut buffer = DeterministicBuffer::new(edge_config());

        let mut ping_outer = buffer.on_enter(ping, 0);
        let mut pong_outer = buffer.on_enter(pong, 10);
        let mut ping_inner = buffer.on_enter(ping, 20);
        let pong_inner = buffer.on_enter(pong, 25);

        let d = buffer.on_leave(&pong_inner, Some(ping), 35);
        ping_inner.child_nanoseconds += d;
        let d = buffer.on_leave(&ping_inner, Some(pong), 40);
        pong_outer.child_nanoseconds += d;
        let d = buffer.on_leave(&pong_outer, Some(ping), 50);
        ping_outer.child_nanoseconds += d;
        buffer.on_leave(&ping_outer, None, 60);

        let window = buffer.drain(&interner);
        let row = |name: &str| {
            window
                .functions
                .iter()
                .find(|row| &*row.function == name)
                .expect("row")
                .clone()
        };
        let ping_row = row("ping");
        let pong_row = row("pong");

        assert_eq!(ping_row.call_count, 2);
        assert_eq!(pong_row.call_count, 2);
        assert_eq!(ping_row.max_recursion_depth, 2);
        assert_eq!(pong_row.max_recursion_depth, 2);
        // ping's outermost frame: 0 -> 60. Banked once, not once per frame.
        assert_eq!(ping_row.inclusive_nanoseconds, 60);
        // pong's outermost frame: 10 -> 50.
        assert_eq!(pong_row.inclusive_nanoseconds, 40);
        // ping exclusive: outer (60 - 40) + inner (20 - 10) = 30.
        assert_eq!(ping_row.exclusive_nanoseconds, 30);
        // pong exclusive: outer (40 - 20) + inner (10 - 0) = 30.
        assert_eq!(pong_row.exclusive_nanoseconds, 30);
        // Every nanosecond of the outermost frame is accounted for exactly once.
        assert_eq!(
            ping_row.exclusive_nanoseconds + pong_row.exclusive_nanoseconds,
            60
        );
    }

    // --- Exclusive time on a plain tree ------------------------------------

    #[test]
    fn a_non_recursive_function_satisfies_inclusive_equals_exclusive_plus_children() {
        let (interner, ids) = interner_with(&["caller", "callee"]);
        let (caller, callee) = (ids[0], ids[1]);
        let mut buffer = DeterministicBuffer::new(edge_config());

        let mut outer = buffer.on_enter(caller, 0);
        let inner = buffer.on_enter(callee, 5);
        let inner_duration = buffer.on_leave(&inner, Some(caller), 25);
        outer.child_nanoseconds += inner_duration;
        buffer.on_leave(&outer, None, 30);

        let window = buffer.drain(&interner);
        let caller_row = window
            .functions
            .iter()
            .find(|row| &*row.function == "caller")
            .expect("caller row");
        assert_eq!(caller_row.max_recursion_depth, 1);
        let children: u64 = caller_row
            .callees
            .iter()
            .map(|edge| edge.inclusive_nanoseconds)
            .sum();
        assert_eq!(
            caller_row.inclusive_nanoseconds,
            caller_row.exclusive_nanoseconds + children
        );
        assert_eq!(caller_row.callees.len(), 1);
        assert_eq!(&*caller_row.callees[0].callee, "callee");
    }

    #[test]
    fn exclusive_time_on_a_three_level_tree_never_double_counts_a_nanosecond() {
        // root(0..100) -> mid(10..80) -> leaf(20..30) and leaf(40..70).
        let (interner, ids) = interner_with(&["root", "mid", "leaf"]);
        let (root, mid, leaf) = (ids[0], ids[1], ids[2]);
        let mut buffer = DeterministicBuffer::new(edge_config());

        let mut root_frame = buffer.on_enter(root, 0);
        let mut mid_frame = buffer.on_enter(mid, 10);
        let first = buffer.on_enter(leaf, 20);
        mid_frame.child_nanoseconds += buffer.on_leave(&first, Some(mid), 30);
        let second = buffer.on_enter(leaf, 40);
        mid_frame.child_nanoseconds += buffer.on_leave(&second, Some(mid), 70);
        root_frame.child_nanoseconds += buffer.on_leave(&mid_frame, Some(root), 80);
        buffer.on_leave(&root_frame, None, 100);

        let window = buffer.drain(&interner);
        let by_name: std::collections::BTreeMap<String, FunctionRow> = window
            .functions
            .iter()
            .map(|row| (row.function.to_string(), row.clone()))
            .collect();

        assert_eq!(by_name["root"].inclusive_nanoseconds, 100);
        assert_eq!(by_name["root"].exclusive_nanoseconds, 30); // 100 - 70
        assert_eq!(by_name["mid"].inclusive_nanoseconds, 70);
        assert_eq!(by_name["mid"].exclusive_nanoseconds, 30); // 70 - (10 + 30)
        assert_eq!(by_name["leaf"].call_count, 2);
        assert_eq!(by_name["leaf"].inclusive_nanoseconds, 40);
        assert_eq!(by_name["leaf"].exclusive_nanoseconds, 40);
        // Total exclusive over the whole tree is exactly the root's wall time.
        let total: u64 = window
            .functions
            .iter()
            .map(|row| row.exclusive_nanoseconds)
            .sum();
        assert_eq!(total, 100);
    }

    // --- The O(distinct functions) claim -----------------------------------

    #[test]
    fn the_buffer_is_sized_by_distinct_functions_not_by_calls() {
        // The property that makes Tier 1 safe to leave on. 10_000 calls, 3 rows.
        let (interner, ids) = interner_with(&["a", "b", "c"]);
        let mut buffer = DeterministicBuffer::new(DeterministicConfig::default());
        for index in 0..10_000u64 {
            let function = ids[(index % 3) as usize];
            let frame = buffer.on_enter(function, index * 2);
            buffer.on_leave(&frame, None, index * 2 + 1);
        }
        let window = buffer.drain(&interner);
        assert_eq!(window.functions.len(), 3);
        let calls: u64 = window.functions.iter().map(|row| row.call_count).sum();
        assert_eq!(calls, 10_000);
        assert!(!window.truncation.truncated_functions());
    }

    #[test]
    fn the_function_cap_reports_rather_than_silently_drops() {
        let config = DeterministicConfig {
            max_functions: 1,
            ..DeterministicConfig::default()
        };
        let (interner, ids) = interner_with(&["kept", "dropped"]);
        let mut buffer = DeterministicBuffer::new(config);
        let first = buffer.on_enter(ids[0], 0);
        buffer.on_leave(&first, None, 10);
        let second = buffer.on_enter(ids[1], 10);
        buffer.on_leave(&second, None, 20);
        let window = buffer.drain(&interner);
        assert_eq!(window.functions.len(), 1);
        assert!(window.truncation.functions_seen > window.truncation.functions_kept);
        assert!(window.truncation.truncated_functions());
    }

    #[test]
    fn a_frame_the_cap_refused_does_not_bank_time_against_another_row() {
        let config = DeterministicConfig {
            max_functions: 1,
            ..DeterministicConfig::default()
        };
        let (interner, ids) = interner_with(&["kept", "dropped"]);
        let mut buffer = DeterministicBuffer::new(config);
        let kept = buffer.on_enter(ids[0], 0);
        buffer.on_leave(&kept, None, 10);
        let refused = buffer.on_enter(ids[1], 10);
        assert!(!refused.counted);
        // Still returns the duration, so the parent's child accounting stays honest.
        assert_eq!(buffer.on_leave(&refused, None, 60), 50);
        let window = buffer.drain(&interner);
        assert_eq!(window.functions.len(), 1);
        assert_eq!(window.functions[0].inclusive_nanoseconds, 10);
    }

    #[test]
    fn a_disarmed_buffer_records_nothing_at_all() {
        let (interner, ids) = interner_with(&["a"]);
        let mut buffer = DeterministicBuffer::new(DeterministicConfig::off());
        let frame = buffer.on_enter(ids[0], 0);
        assert!(!frame.counted);
        assert_eq!(buffer.on_leave(&frame, None, 100), 100);
        let window = buffer.drain(&interner);
        assert!(window.is_empty());
        assert!(!window.coverage.aggregates);
    }

    // --- Tier 2 ------------------------------------------------------------

    #[test]
    fn edges_aggregate_per_caller_callee_pair() {
        let (interner, ids) = interner_with(&["controller", "repository", "pdo"]);
        let (controller, repository, pdo) = (ids[0], ids[1], ids[2]);
        let mut buffer = DeterministicBuffer::new(edge_config());

        // controller calls repository twice; repository calls pdo once each time.
        let mut clock = 0u64;
        let mut controller_frame = buffer.on_enter(controller, clock);
        for _ in 0..2 {
            clock += 1;
            let mut repository_frame = buffer.on_enter(repository, clock);
            clock += 1;
            let pdo_frame = buffer.on_enter(pdo, clock);
            clock += 5;
            repository_frame.child_nanoseconds +=
                buffer.on_leave(&pdo_frame, Some(repository), clock);
            clock += 1;
            controller_frame.child_nanoseconds +=
                buffer.on_leave(&repository_frame, Some(controller), clock);
        }
        buffer.on_leave(&controller_frame, None, clock + 1);

        let window = buffer.drain(&interner);
        assert!(window.coverage.edges);
        let repository_row = window
            .functions
            .iter()
            .find(|row| &*row.function == "repository")
            .expect("repository row");
        assert_eq!(repository_row.callees.len(), 1);
        assert_eq!(&*repository_row.callees[0].callee, "pdo");
        assert_eq!(repository_row.callees[0].call_count, 2);
        assert_eq!(repository_row.callees[0].inclusive_nanoseconds, 10);

        let controller_row = window
            .functions
            .iter()
            .find(|row| &*row.function == "controller")
            .expect("controller row");
        assert_eq!(controller_row.callees.len(), 1);
        assert_eq!(controller_row.callees[0].call_count, 2);
    }

    #[test]
    fn edges_are_absent_when_tier_two_is_off() {
        // Absent, not empty: "Tier 2 is off" and "this function calls nothing" must not
        // be representable by the same document.
        let (interner, ids) = interner_with(&["a", "b"]);
        let mut buffer = DeterministicBuffer::new(DeterministicConfig::default());
        let outer = buffer.on_enter(ids[0], 0);
        let inner = buffer.on_enter(ids[1], 1);
        buffer.on_leave(&inner, Some(ids[0]), 2);
        buffer.on_leave(&outer, None, 3);
        let window = buffer.drain(&interner);
        assert!(!window.coverage.edges);
        assert!(window.functions.iter().all(|row| row.callees.is_empty()));
        assert_eq!(window.truncation.edges_kept, 0);
    }

    #[test]
    fn the_edge_cap_reports_rather_than_silently_drops() {
        let config = DeterministicConfig {
            edges: true,
            max_edges: 1,
            ..DeterministicConfig::default()
        };
        let (interner, ids) = interner_with(&["caller", "first", "second"]);
        let mut buffer = DeterministicBuffer::new(config);
        let caller = buffer.on_enter(ids[0], 0);
        for callee in [ids[1], ids[2]] {
            let frame = buffer.on_enter(callee, 1);
            buffer.on_leave(&frame, Some(ids[0]), 2);
        }
        buffer.on_leave(&caller, None, 10);
        let window = buffer.drain(&interner);
        assert_eq!(window.truncation.edges_kept, 1);
        assert!(window.truncation.edges_seen > window.truncation.edges_kept);
    }

    // --- Tier 3 ------------------------------------------------------------

    fn argument(name: &str, argument_type: ArgumentType, value: &str) -> CapturedArgument {
        CapturedArgument {
            position: 0,
            name: name.to_owned(),
            argument_type,
            value: value.to_owned(),
            redacted: false,
            truncated: false,
        }
    }

    fn armed_config() -> DeterministicConfig {
        DeterministicConfig {
            arguments: true,
            ..DeterministicConfig::default()
        }
    }

    #[test]
    fn tier_three_refuses_until_armed() {
        // Two independent gates: the flag says "allowed", arming says "this request".
        let (_interner, ids) = interner_with(&["App\\Orders\\Service::purchaseLabel"]);
        let mut buffer = DeterministicBuffer::new(armed_config());
        let orders = argument("orderId", ArgumentType::Int, "4711");
        assert!(!buffer.record_arguments(ids[0], vec![orders.clone()], 0));
        assert!(!buffer.arguments_active());
        buffer.arm_arguments();
        assert!(buffer.arguments_active());
        assert!(buffer.record_arguments(ids[0], vec![orders], 0));
    }

    #[test]
    fn tier_three_records_nothing_when_the_flag_is_off_however_armed() {
        let (_interner, ids) = interner_with(&["App\\Orders\\Service::purchaseLabel"]);
        let mut buffer = DeterministicBuffer::new(DeterministicConfig::default());
        buffer.arm_arguments();
        assert!(!buffer.arguments_active());
        assert!(!buffer.record_arguments(ids[0], vec![argument("a", ArgumentType::Int, "1")], 0));
    }

    #[test]
    fn tier_three_appears_only_on_the_function_it_was_recorded_for() {
        // The allowlist itself lives in the observer; what this proves is that a
        // recorded sample never bleeds onto a neighbouring row.
        let (interner, ids) = interner_with(&["allowlisted", "not_allowlisted"]);
        let mut buffer = DeterministicBuffer::new(armed_config());
        buffer.arm_arguments();
        for id in &ids {
            let frame = buffer.on_enter(*id, 0);
            buffer.on_leave(&frame, None, 10);
        }
        assert!(buffer.record_arguments(ids[0], vec![argument("q", ArgumentType::Str, "x")], 0));
        let window = buffer.drain(&interner);
        let allowlisted = window
            .functions
            .iter()
            .find(|row| &*row.function == "allowlisted")
            .expect("row");
        let other = window
            .functions
            .iter()
            .find(|row| &*row.function == "not_allowlisted")
            .expect("row");
        assert_eq!(allowlisted.argument_samples.len(), 1);
        assert!(other.argument_samples.is_empty());
    }

    #[test]
    fn the_invocation_cap_bounds_retained_samples_but_keeps_counting_invocations() {
        let config = DeterministicConfig {
            arguments: true,
            max_argument_invocations: 2,
            ..DeterministicConfig::default()
        };
        let (interner, ids) = interner_with(&["hot"]);
        let mut buffer = DeterministicBuffer::new(config);
        buffer.arm_arguments();
        let frame = buffer.on_enter(ids[0], 0);
        buffer.on_leave(&frame, None, 1);
        let accepted: Vec<bool> = (0..5)
            .map(|_| {
                buffer.record_arguments(ids[0], vec![argument("n", ArgumentType::Int, "1")], 0)
            })
            .collect();
        assert_eq!(accepted, vec![true, true, false, false, false]);
        let window = buffer.drain(&interner);
        let samples = &window.functions[0].argument_samples;
        assert_eq!(samples.len(), 2);
        // 1-based and truthful about which invocations these were.
        assert_eq!(samples[0].invocation, 1);
        assert_eq!(samples[1].invocation, 2);
    }

    #[test]
    fn the_argument_count_cap_truncates_and_reports() {
        let config = DeterministicConfig {
            arguments: true,
            max_arguments: 2,
            ..DeterministicConfig::default()
        };
        let (interner, ids) = interner_with(&["wide"]);
        let mut buffer = DeterministicBuffer::new(config);
        buffer.arm_arguments();
        let frame = buffer.on_enter(ids[0], 0);
        buffer.on_leave(&frame, None, 1);
        let arguments = vec![
            argument("a", ArgumentType::Int, "1"),
            argument("b", ArgumentType::Int, "2"),
            argument("c", ArgumentType::Int, "3"),
            argument("d", ArgumentType::Int, "4"),
        ];
        assert!(buffer.record_arguments(ids[0], arguments, 0));
        let window = buffer.drain(&interner);
        let sample = &window.functions[0].argument_samples[0];
        assert_eq!(sample.arguments.len(), 2);
        assert_eq!(sample.arguments_dropped, 2);
    }

    #[test]
    fn the_total_byte_budget_refuses_and_reports_rather_than_clipping_silently() {
        let config = DeterministicConfig {
            arguments: true,
            max_argument_total_bytes: 8,
            ..DeterministicConfig::default()
        };
        let (interner, ids) = interner_with(&["chatty"]);
        let mut buffer = DeterministicBuffer::new(config);
        buffer.arm_arguments();
        let frame = buffer.on_enter(ids[0], 0);
        buffer.on_leave(&frame, None, 1);
        assert!(buffer.record_arguments(
            ids[0],
            vec![argument("a", ArgumentType::Str, "12345678")],
            0
        ));
        assert!(!buffer.record_arguments(ids[0], vec![argument("b", ArgumentType::Str, "9")], 0));
        let window = buffer.drain(&interner);
        assert_eq!(window.functions[0].argument_samples.len(), 1);
        assert!(window.truncation.argument_bytes_dropped > 0);
        assert!(window.truncation.truncated_arguments());
    }

    #[test]
    fn composite_types_never_carry_a_value() {
        for composite in [
            ArgumentType::Object,
            ArgumentType::Array,
            ArgumentType::Resource,
        ] {
            assert!(!composite.carries_value());
            // Even when a value is offered — the refusal is structural, not a policy
            // the caller can decline to apply.
            let captured = capture_argument(
                3,
                "options".to_owned(),
                composite,
                Some("{\"secret\":1}".to_owned()),
                false,
                256,
            );
            assert_eq!(captured.value, "");
            assert!(!captured.redacted);
            assert!(!captured.truncated);
        }
        for scalar in [
            ArgumentType::Null,
            ArgumentType::Bool,
            ArgumentType::Int,
            ArgumentType::Float,
            ArgumentType::Str,
        ] {
            assert!(scalar.carries_value());
        }
    }

    #[test]
    fn a_redacted_scalar_carries_the_mask_and_says_so() {
        let captured = capture_argument(
            2,
            "apiToken".to_owned(),
            ArgumentType::Str,
            Some("live_sk_abcdef".to_owned()),
            true,
            256,
        );
        assert_eq!(captured.value, crate::http_capture::MASK);
        assert!(captured.redacted);
        assert!(!captured.value.contains("live_sk"));
    }

    #[test]
    fn an_over_long_scalar_is_clipped_on_a_utf8_boundary_and_flagged() {
        // A byte-offset truncate would panic mid-codepoint, and panic = "abort" is set
        // for release: that would take the worker down on a multi-byte argument.
        let value = "é".repeat(8); // 16 bytes
        let captured = capture_argument(
            0,
            "label".to_owned(),
            ArgumentType::Str,
            Some(value),
            false,
            5,
        );
        assert!(captured.truncated);
        assert_eq!(captured.value, "éé");
        assert!(captured.value.len() <= 5);
    }

    #[test]
    fn a_null_argument_carries_its_type_and_no_value() {
        let captured =
            capture_argument(0, "maybe".to_owned(), ArgumentType::Null, None, false, 256);
        assert_eq!(
            captured.argument_type.proto_name(),
            "DETERMINISTIC_ARGUMENT_TYPE_NULL"
        );
        assert_eq!(captured.value, "");
        assert!(!captured.redacted);
    }

    // --- Coverage and configuration ----------------------------------------

    #[test]
    fn coverage_is_declared_rather_than_inferred() {
        let mut buffer = DeterministicBuffer::new(edge_config());
        let coverage = buffer.coverage();
        assert!(coverage.aggregates);
        assert!(coverage.edges);
        assert!(!coverage.arguments);
        // Tier 2 cannot be covered while Tier 1 is not: an edge with no function row to
        // hang off has no denominator and no home.
        buffer.reset(DeterministicConfig {
            aggregates: false,
            edges: true,
            ..DeterministicConfig::default()
        });
        assert!(!buffer.coverage().edges);
    }

    #[test]
    fn defaults_are_tier_one_on_and_everything_else_off() {
        let config = DeterministicConfig::default();
        assert!(config.aggregates, "Tier 1 is the one flag that defaults ON");
        assert!(!config.edges);
        assert!(!config.arguments);
        assert_eq!(config.max_functions, 4_096);
        assert_eq!(config.max_edges, 8_192);
        assert_eq!(config.max_arguments, 8);
        assert_eq!(config.max_argument_bytes, 256);
        assert_eq!(config.max_argument_total_bytes, 4_096);
        assert_eq!(config.max_argument_invocations, 4);
    }

    #[test]
    fn the_off_posture_is_every_tier_disabled() {
        let config = DeterministicConfig::off();
        assert!(!config.aggregates);
        assert!(!config.edges);
        assert!(!config.arguments);
    }

    #[test]
    fn caps_are_clamped_so_a_written_zero_cannot_produce_an_empty_covered_window() {
        let clamped = DeterministicConfig {
            max_functions: 0,
            max_edges: 0,
            ..DeterministicConfig::default()
        }
        .clamped();
        assert_eq!(clamped.max_functions, 1);
        assert_eq!(clamped.max_edges, 1);
        let huge = DeterministicConfig {
            max_functions: usize::MAX,
            max_edges: usize::MAX,
            max_arguments: usize::MAX,
            max_argument_bytes: usize::MAX,
            max_argument_total_bytes: usize::MAX,
            max_argument_invocations: usize::MAX,
            ..DeterministicConfig::default()
        }
        .clamped();
        assert_eq!(huge.max_functions, 262_144);
        assert_eq!(huge.max_edges, 1_048_576);
        assert_eq!(huge.max_arguments, 64);
        assert_eq!(huge.max_argument_bytes, 8_192);
        assert_eq!(huge.max_argument_total_bytes, 262_144);
        assert_eq!(huge.max_argument_invocations, 256);
        // A zero argument cap is a coherent "types only" posture and is preserved.
        let types_only = DeterministicConfig {
            max_arguments: 0,
            ..DeterministicConfig::default()
        }
        .clamped();
        assert_eq!(types_only.max_arguments, 0);
    }

    /// `settings::get` reads process env, so every test that touches it has to
    /// serialise against every other one.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn config_resolves_from_the_environment_including_the_kill_switch() {
        let _guard = env_lock().lock().unwrap_or_else(|error| error.into_inner());
        for name in [
            "CHRONOS_PHP_PROFILE_DETERMINISTIC",
            "CHRONOS_PHP_PROFILE_DETERMINISTIC_MAX_FUNCTIONS",
            "CHRONOS_PHP_PROFILE_EDGES",
            "CHRONOS_PHP_PROFILE_EDGES_MAX",
            "CHRONOS_PHP_PROFILE_ARGS",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_ARGS",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_ARG_BYTES",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_TOTAL_BYTES",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_INVOCATIONS",
        ] {
            std::env::remove_var(name);
        }

        // Nothing written at all: Tier 1 on, Tiers 2 and 3 off.
        let defaults = DeterministicConfig::resolve();
        assert!(defaults.aggregates);
        assert!(!defaults.edges);
        assert!(!defaults.arguments);
        assert_eq!(defaults.max_functions, 4_096);

        // The kill switch, with no deploy.
        std::env::set_var("CHRONOS_PHP_PROFILE_DETERMINISTIC", "0");
        assert!(!DeterministicConfig::resolve().aggregates);
        std::env::set_var("CHRONOS_PHP_PROFILE_DETERMINISTIC", "true");
        assert!(DeterministicConfig::resolve().aggregates);

        // Opt-ins and their caps.
        std::env::set_var("CHRONOS_PHP_PROFILE_EDGES", "yes");
        std::env::set_var("CHRONOS_PHP_PROFILE_EDGES_MAX", "16");
        std::env::set_var("CHRONOS_PHP_PROFILE_ARGS", "on");
        std::env::set_var("CHRONOS_PHP_PROFILE_ARGS_MAX_ARGS", "3");
        std::env::set_var("CHRONOS_PHP_PROFILE_ARGS_MAX_ARG_BYTES", "32");
        std::env::set_var("CHRONOS_PHP_PROFILE_ARGS_MAX_TOTAL_BYTES", "64");
        std::env::set_var("CHRONOS_PHP_PROFILE_ARGS_MAX_INVOCATIONS", "2");
        std::env::set_var("CHRONOS_PHP_PROFILE_DETERMINISTIC_MAX_FUNCTIONS", "7");
        let resolved = DeterministicConfig::resolve();
        assert!(resolved.edges);
        assert_eq!(resolved.max_edges, 16);
        assert!(resolved.arguments);
        assert_eq!(resolved.max_arguments, 3);
        assert_eq!(resolved.max_argument_bytes, 32);
        assert_eq!(resolved.max_argument_total_bytes, 64);
        assert_eq!(resolved.max_argument_invocations, 2);
        assert_eq!(resolved.max_functions, 7);

        for name in [
            "CHRONOS_PHP_PROFILE_DETERMINISTIC",
            "CHRONOS_PHP_PROFILE_DETERMINISTIC_MAX_FUNCTIONS",
            "CHRONOS_PHP_PROFILE_EDGES",
            "CHRONOS_PHP_PROFILE_EDGES_MAX",
            "CHRONOS_PHP_PROFILE_ARGS",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_ARGS",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_ARG_BYTES",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_TOTAL_BYTES",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_INVOCATIONS",
        ] {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn every_setting_this_module_reads_is_registered_for_ini_and_dotchronos() {
        // Unregistered names still work through process env, which is exactly what makes
        // this omission invisible until an operator tries php.ini or a `.chronos` file.
        for name in [
            "CHRONOS_PHP_PROFILE_DETERMINISTIC",
            "CHRONOS_PHP_PROFILE_DETERMINISTIC_MAX_FUNCTIONS",
            "CHRONOS_PHP_PROFILE_EDGES",
            "CHRONOS_PHP_PROFILE_EDGES_MAX",
            "CHRONOS_PHP_PROFILE_ARGS",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_ARGS",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_ARG_BYTES",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_TOTAL_BYTES",
            "CHRONOS_PHP_PROFILE_ARGS_MAX_INVOCATIONS",
        ] {
            assert!(
                crate::settings::SETTING_NAMES.contains(&name),
                "{name} is missing from settings::SETTING_NAMES"
            );
        }
    }

    #[test]
    fn the_fast_tier_three_gate_mirrors_the_buffer_and_is_cleared_by_a_reset() {
        // Two writers, one answer. If the Cell ever disagreed with the buffer, argument
        // capture would either silently stop or silently continue into the next request.
        reset_request(DeterministicConfig {
            arguments: true,
            ..DeterministicConfig::default()
        });
        assert!(!arguments_active(), "the flag alone must not arm anything");
        arm_arguments();
        assert!(arguments_active());
        // A new request re-derives it, so arming never leaks across requests.
        reset_request(DeterministicConfig {
            arguments: true,
            ..DeterministicConfig::default()
        });
        assert!(!arguments_active());
        // And the kill-switch posture leaves it off however armed.
        reset_request(DeterministicConfig::off());
        arm_arguments();
        assert!(!arguments_active());
        reset_request(DeterministicConfig::off());
    }

    #[test]
    fn the_derived_ini_and_file_spellings_are_the_documented_ones() {
        assert_eq!(
            crate::settings::ini_name("CHRONOS_PHP_PROFILE_EDGES"),
            "chronos.profile_edges"
        );
        assert_eq!(
            crate::settings::short_key("CHRONOS_PHP_PROFILE_DETERMINISTIC"),
            "profile_deterministic"
        );
    }
}
