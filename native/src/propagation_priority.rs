//! Propagation priority: when Chronos is collecting, Chronos owns the `traceparent`.
//!
//! A process can carry more than one tracer. Estates migrating onto Chronos run
//! ddtrace alongside it for a while, and both extensions hook `curl_exec` to
//! inject W3C trace context. Ours lands first (the Zend observer's BEGIN handler
//! runs before the function's own handler); ddtrace injects from INSIDE the
//! `curl_exec` handler, so its header is the last writer and ours never reaches
//! the wire.
//!
//! The consequence is not cosmetic. The callee adopts the header it was given,
//! so its server span joins the FOREIGN tracer's trace id while the client span
//! we recorded sits under ours. Caller and callee end up in two different traces,
//! and every read path that joins a trace — the dependency graph, hop banding,
//! the waterfall — sees two disconnected halves. Measured on the local estate
//! (2026-09-14): deepwell's calls to `service-auth` and `mercury` were invisible
//! as edges for exactly this reason, while both services were plainly reporting.
//!
//! So where Chronos is instrumenting, Chronos takes priority: before our first
//! outbound call of a request we turn OFF the other tracer's distributed-tracing
//! injection for the remainder of that request. Three properties make this a
//! narrow intervention rather than a hostile one:
//!
//!   * It is per request. The knobs below are `PHP_INI_ALL`, so the change is a
//!     runtime modification that Zend restores at request shutdown — nothing is
//!     written to anybody's configuration.
//!   * It disables the other tracer's PROPAGATION only. Its own spans, its own
//!     agent, its own sampling keep working exactly as before; it simply stops
//!     rewriting a header it did not originate.
//!   * It is opt-out, via `CHRONOS_PHP_PROPAGATION_PRIORITY=false`, for an estate
//!     that deliberately wants the other tracer to own the wire during a
//!     migration. Chronos then keeps recording its own spans and the graph falls
//!     back to the first-party `chronos.peer.application` stamp on the read side.
//!
//! Deliberately NOT done at RINIT: another extension's request-init may run after
//! ours and re-read its configuration, and our own RINIT is a worse place to call
//! into userland anyway. Claiming lazily at the first outbound call is both later
//! than every RINIT and exactly when the header is about to be written.

/// Opt-out switch. Default ON: a process running two tracers is already paying
/// for both, and the one being asked to own the trace is the one being installed.
pub const PRIORITY_FLAG: &str = "CHRONOS_PHP_PROPAGATION_PRIORITY";

/// Foreign-tracer INI settings that turn OFF distributed-context injection, and
/// the value that means "off". Every entry must be `PHP_INI_ALL` (runtime
/// changeable) and must disable PROPAGATION ONLY — never the other tracer's
/// span collection, which is not ours to switch off.
///
/// `datadog.distributed_tracing` (`DD_DISTRIBUTED_TRACING`) is the only entry
/// today: it is the single switch that stops ddtrace injecting both its
/// `x-datadog-*` headers and its `traceparent`/`tracestate` pair.
pub const FOREIGN_PROPAGATION_SETTINGS: &[(&str, &str)] = &[("datadog.distributed_tracing", "0")];

/// Is an INI value already "off"? PHP spells a disabled boolean INI several ways
/// depending on how it was written — `ini_get` on ddtrace returns the literal
/// `"true"`/`"false"` it was configured with, while a numeric config reads back
/// as `"1"`/`"0"` and an unset one as the empty string.
///
/// Anything unrecognised counts as ON, so an unfamiliar spelling makes us claim
/// priority (a redundant `ini_set` is harmless) rather than silently skip.
#[must_use]
pub fn reads_as_off(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "off" | "no" | "false"
    )
}

/// The decision, kept pure so it can be tested without a PHP runtime: claim
/// priority when the operator has not opted out and the setting is present and
/// currently enabled. `current` is `None` when the INI does not exist, i.e. the
/// other tracer is not loaded at all.
#[must_use]
pub fn should_claim(priority_enabled: bool, current: Option<&str>) -> bool {
    priority_enabled && current.is_some_and(|value| !reads_as_off(value))
}

#[cfg(test)]
mod tests {
    use super::{reads_as_off, should_claim};

    #[test]
    fn recognises_every_spelling_of_off() {
        for value in ["", "0", "off", "no", "false", "FALSE", " Off "] {
            assert!(reads_as_off(value), "{value:?} should read as off");
        }
    }

    #[test]
    fn anything_else_is_on() {
        for value in ["1", "true", "On", "yes", "enabled-somehow"] {
            assert!(!reads_as_off(value), "{value:?} should read as on");
        }
    }

    #[test]
    fn claims_when_the_other_tracer_is_loaded_and_propagating() {
        assert!(should_claim(true, Some("true")));
        assert!(should_claim(true, Some("1")));
    }

    #[test]
    fn does_not_claim_when_opted_out() {
        assert!(!should_claim(false, Some("true")));
    }

    #[test]
    fn does_not_claim_when_the_other_tracer_is_absent_or_already_quiet() {
        assert!(!should_claim(true, None));
        assert!(!should_claim(true, Some("false")));
        assert!(!should_claim(true, Some("0")));
    }
}

use std::cell::Cell;

thread_local! {
    /// Claimed once per request: the INI write is request-scoped, so repeating it
    /// on every outbound call would be pure overhead. Reset by
    /// [`reset_for_request`] from the observer's request-context setup.
    static CLAIMED: Cell<bool> = const { Cell::new(false) };
}

/// Forget the per-request claim. Called wherever a request's observer state is
/// (re)initialised, so a worker opening thousands of message-scoped requests in
/// one process claims once per request rather than once per process.
pub fn reset_for_request() {
    CLAIMED.with(|claimed| claimed.set(false));
}

/// Take ownership of outbound trace propagation for this request, once.
///
/// Called from the observer immediately before we inject a `traceparent`, which
/// is the first moment that is both later than every extension's RINIT and
/// earlier than the header reaching the wire. Every step fails open: a missing
/// `ini_get`/`ini_set`, an INI another tracer does not define, or a refused write
/// all leave the request exactly as it was.
pub fn claim_once() {
    if CLAIMED.with(|claimed| claimed.replace(true)) {
        return;
    }
    if !crate::settings::flag(PRIORITY_FLAG, true) {
        return;
    }
    for (setting, off_value) in FOREIGN_PROPAGATION_SETTINGS {
        let current = ini_value(setting);
        if !should_claim(true, current.as_deref()) {
            continue;
        }
        set_ini(setting, off_value);
    }
}

/// `ini_get($setting)`, as `None` when the INI is not registered (the other
/// tracer is not loaded) and `Some(value)` otherwise.
fn ini_value(setting: &str) -> Option<String> {
    let function = ext_php_rs::zend::Function::try_from_function("ini_get")?;
    let result = function.try_call(vec![&setting]).ok()?;
    // A missing INI returns `false`, which reads back as no string.
    result.str().map(str::to_owned)
}

/// `ini_set($setting, $value)`, return value deliberately ignored: a refusal
/// means the other tracer pinned the setting, and the honest outcome is that its
/// header wins — not an error on a request that is only trying to trace.
fn set_ini(setting: &str, value: &str) {
    if let Some(function) = ext_php_rs::zend::Function::try_from_function("ini_set") {
        let _ = function.try_call(vec![&setting, &value]);
    }
}
