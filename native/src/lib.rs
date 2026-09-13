//! Chronos native PHP collector extension.
//!
//! The `.so` owns all heavy collection: APM (Zend observer spans), statistical profiling,
//! structured log capture, DST recording, runtime metrics, and all spool I/O. The PHP
//! package is reduced to a thin span-decoration API for userland instrumentation.
//!
//! Request lifecycle:
//!   chronos_request_start -> resolve config, parse trace context, head-sample, arm profiler, DST
//!   fcall  -> observer begin/end handlers capture spans (Zend engine level)
//!   tick   -> SIGPROF queues a tick; the observer boundary walks the stack
//!   chronos_request_end -> flush spans, profiles, logs, DST, metrics to spool directory
//!
//! The userland SDK additionally registers a shutdown function so a fatal error still
//! flushes through `chronos_request_end` (which is idempotent — second calls no-op).
//!
//! The `zend-observer` feature gate controls unsafe FFI. Without it the extension loads
//! inert (smoke-testable on any PHP version).

// Without `zend-observer` the observer's begin/end trampolines are not compiled, so
// everything only they call — `CallFrame`, `on_begin`, `on_end`, the span clocks — reads
// as dead. That build exists to smoke-load the extension on an unsupported PHP and to run
// the crate's pure unit tests; treating its unused-code warnings as findings would mean
// gating half the observer on a feature it has nothing to do with. The DEFAULT build
// still warns normally, which is the build that ships.
#![cfg_attr(not(feature = "zend-observer"), allow(dead_code))]

use ext_php_rs::prelude::*;

pub mod body_spool;
pub mod call_path;
pub mod config;
pub mod context;
pub mod deterministic;
pub mod deterministic_spool;
pub mod dst_spool;
pub mod http_capture;
pub mod job_spool;
pub mod log_spool;
pub mod messaging;
pub mod observer;
pub mod profile_spool;
pub mod rate;
pub mod replay_hooks;
pub mod request_attributes;
pub mod sampler;
pub mod settings;
pub mod spool;
pub mod spool_common;
pub mod spool_log;
pub mod vcs;

use config::CollectorConfig;
use context::{CollectorEnvelope, TraceContext};
use rand::Rng;
use std::cell::RefCell;

thread_local! {
    static REQUEST_CONFIG: RefCell<Option<CollectorConfig>> = const { RefCell::new(None) };
    static REQUEST_CONTEXT: RefCell<Option<TraceContext>> = const { RefCell::new(None) };
    static REQUEST_ENVELOPE: RefCell<Option<CollectorEnvelope>> = const { RefCell::new(None) };
    static REQUEST_START_NS: RefCell<u128> = const { RefCell::new(0) };
    static REQUEST_STARTED_AT: RefCell<String> = const { RefCell::new(String::new()) };
    static REQUEST_HTTP_METHOD: RefCell<String> = const { RefCell::new(String::new()) };
    static REQUEST_SERVICE_NAME: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Once-per-process heartbeat so a wired-but-idle collector is diagnosable: every
/// silent failure mode so far (cleared FPM env, wrong extension name, missing SDK)
/// looked identical to "healthy but no traffic".
static HEARTBEAT_SENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn startup(_ty: i32, mod_num: i32) -> i32 {
    settings::register_ini_entries(mod_num);
    observer::install_observer();
    0
}

/// RINIT: start the request natively, before any PHP has run. This is what makes the
/// composer package optional — with no SDK installed at all, every web request still
/// gets its trace context, root span, observer I/O spans, HTTP capture and profile.
/// When the SDK IS installed, its later `chronos_request_start` call finds the
/// request open and enriches it instead (see `chronos_request_start`).
unsafe extern "C" fn request_startup(_ty: i32, _mod_num: i32) -> i32 {
    native_request_start();
    0
}

/// RSHUTDOWN: flush whatever is still open. Userland shutdown functions (the SDK's
/// fatal-error net) run BEFORE module RSHUTDOWN, so when the SDK is present this is
/// a no-op on the already-ended request; without the SDK it is the flush.
unsafe extern "C" fn request_shutdown(_ty: i32, _mod_num: i32) -> i32 {
    let status = i64::from(
        ext_php_rs::zend::SapiGlobals::get()
            .sapi_headers()
            .http_response_code,
    );
    chronos_request_end(status, String::new(), None, None, None, None, None, None);
    0
}

fn native_request_start() {
    let config = CollectorConfig::resolve();
    if !config.enabled || config.envelope.is_none() {
        return;
    }

    let server = http_capture::server_vars();
    let method = http_capture::lookup(&server, "REQUEST_METHOD").to_owned();
    if method.is_empty() {
        // No HTTP request means CLI or a worker. Auto-starting there would wrap a
        // whole long-running process in one request, so it is opt-in; the SDK can
        // still start (and end) worker "requests" explicitly at any time.
        if !settings::flag("CHRONOS_PHP_CLI_ENABLED", false) {
            return;
        }
    }

    let traceparent = http_capture::lookup(&server, "HTTP_TRACEPARENT").to_owned();
    // The traceparent's two companion headers, read in the same breath so the
    // frameworkless path propagates everything a bridge-started request would.
    let tracestate = http_capture::lookup(&server, "HTTP_TRACESTATE").to_owned();
    let baggage = http_capture::lookup(&server, "HTTP_BAGGAGE").to_owned();
    let session_id = http_capture::lookup(&server, "HTTP_X_CHRONOS_SESSION_ID").to_owned();
    let dst_directive = {
        let header = http_capture::lookup(&server, "HTTP_X_CHRONOS_DST");
        if header.is_empty() {
            cookie_value(http_capture::lookup(&server, "HTTP_COOKIE"), "chronos_dst")
        } else {
            header.to_owned()
        }
    };
    // The forced-profile directive, read the same way and in the same breath:
    // header first, then cookie, so a browser session can carry it hands-free
    // once while a one-off curl can pass it per request.
    let profile_directive = {
        let header = http_capture::lookup(&server, "HTTP_X_CHRONOS_PROFILE");
        if header.is_empty() {
            cookie_value(
                http_capture::lookup(&server, "HTTP_COOKIE"),
                "chronos_profile",
            )
        } else {
            header.to_owned()
        }
    };

    start_request(
        &traceparent,
        &tracestate,
        &baggage,
        &session_id,
        &dst_directive,
        &profile_directive,
        method,
        "",
        String::new(),
    );
}

/// Extract one cookie's value from a raw `Cookie:` header line.
fn cookie_value(raw: &str, name: &str) -> String {
    raw.split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_owned())
        .unwrap_or_default()
}

/// Does this request's profile directive arm a forced profile?
///
/// The directive must carry the configured shared secret. An EMPTY `token`
/// disables the mechanism outright — that is the default, and it is why a
/// service has to opt in before any header can make it do extra work. Compared
/// in constant time: the comparison is against a secret, and a length-or-prefix
/// early exit leaks it a byte at a time to anyone who can time responses.
///
/// The value may be the bare token or `profile=<token>`, so the directive can
/// ride the same `a=b;c=d` shape the DST one uses when a cookie carries both.
fn profile_forced(directive: &str, token: &str) -> bool {
    if token.is_empty() || directive.is_empty() {
        return false;
    }
    directive.split(&[';', ','][..]).any(|part| {
        let part = part.trim();
        let offered = part.strip_prefix("profile=").unwrap_or(part);
        constant_time_eq(offered.as_bytes(), token.as_bytes())
    })
}

/// Length-independent byte comparison. Returns false for differing lengths, but
/// only after a fixed-cost pass over the offered value, so neither the answer
/// nor the timing narrows the secret.
fn constant_time_eq(offered: &[u8], secret: &[u8]) -> bool {
    let mut difference = u8::from(offered.len() != secret.len());
    for (index, byte) in offered.iter().enumerate() {
        // Index into the secret cyclically on a length mismatch: the loop must
        // not shorten to the common prefix, which is what would time-leak it.
        difference |= byte ^ secret[index % secret.len().max(1)];
    }
    difference == 0 && !secret.is_empty()
}

/// Roll a resolved rate's die: `parts` faces out of `denominator` sample.
fn head_sample(rate: rate::SampleRate) -> bool {
    if rate.parts == 0 {
        return false;
    }
    if rate.parts >= rate.denominator {
        return true;
    }
    rand::thread_rng().gen_range(0..rate.denominator) < rate.parts
}

/// The HTTP verbs a real web request arrives with. Anything else in the method
/// slot is a BACKGROUND JOB — `QUEUE` from the Laravel bridge, an empty string
/// from a native CLI start, `CRON`/`CONSUME` from whatever bridge lands next.
///
/// Deliberately a positive list of web verbs rather than a list of job words:
/// a new job vocabulary that nobody remembered to register here would otherwise
/// be silently profiled at the WEB rate, which is zero by default — a worker
/// that quietly reports nothing is exactly the failure this default exists to
/// prevent, and a new HTTP verb is far rarer than a new kind of worker.
const WEB_METHODS: [&str; 9] = [
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

fn is_background_job(http_method: &str) -> bool {
    let method = http_method.trim();
    !WEB_METHODS
        .iter()
        .any(|verb| verb.eq_ignore_ascii_case(method))
}

fn heartbeat(config: &CollectorConfig) {
    if HEARTBEAT_SENT.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // Rates are printed as the fraction actually IN FORCE, not as written. A
    // value quantised away by the resolution, or read back as legacy basis
    // points, differs from what the file says — and the whole point of saying
    // it out loud once per process is that the difference is findable in a log
    // rather than in a bill.
    let legacy = [
        ("apm_sample_rate", config.apm_sample_rate),
        ("profile_request_rate", config.profile_request_rate),
        ("profile_job_rate", config.profile_job_rate),
    ]
    .iter()
    .filter(|(_, rate)| rate.spelling == rate::Spelling::LegacyBasisPoints)
    .map(|(name, _)| *name)
    .collect::<Vec<_>>();
    let summary = format!(
        "chronos-collector active: apm={} apm_rate={} logs={} profiler={} \
         profile_request_rate={} profile_job_rate={} rate_denominator={} dst={}{}",
        config.apm_enabled,
        config.apm_sample_rate.effective_fraction(),
        config.logs_enabled,
        config.profiler_enabled,
        config.profile_request_rate.effective_fraction(),
        config.profile_job_rate.effective_fraction(),
        config.apm_sample_rate.denominator,
        config.dst_enabled,
        if legacy.is_empty() {
            String::new()
        } else {
            format!(
                " | read as legacy basis points (write these as fractions, 0.1 = a tenth): {}",
                legacy.join(", ")
            )
        },
    );
    // A process whose observer never registered still collects — the bridges, the
    // RINIT root span and HTTP capture all work — but every span the Zend observer
    // would have produced is missing. That is invisible otherwise, so it is said
    // here, where the one line per process that describes the collector already is.
    let summary = if observer::installed() {
        summary
    } else {
        format!(
            "{summary} | observer NOT installed: the collector was switched off at \
             module startup, so this process emits no observer spans (I/O, manifest) \
             however it is configured now — see CHRONOS_PHP_ENABLED / chronos.enabled"
        )
    };
    // Lands in the SAPI error log (docker logs) even when log shipping is off.
    eprintln!("[chronos-ext] {summary}");
    if config.logs_enabled {
        log_spool::capture(log_spool::LogRecord {
            severity_text: "INFO".into(),
            severity_number: 9,
            body: summary,
            trace_id: String::new(),
            span_id: String::new(),
            observed_at: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.6fZ")
                .to_string(),
            attributes: vec![("chronos.heartbeat".into(), "true".into())],
        });
    }
}

/// PHP-callable: signal request start with HTTP context.
///
/// Since the native RINIT hook landed, a web request is usually ALREADY started by
/// the time the SDK's framework bridge calls this — in that case the call enriches
/// the open request (session id, DST directive, method/service, profile labels)
/// instead of re-minting a trace context, which would orphan every span the
/// observer has recorded during framework bootstrap. CLI/worker starts (which
/// RINIT skips by default) still take the full path.
// The arity is the PHP-side calling contract (`NativeExtension::requestStart`), not
// a choice this crate gets to make smaller.
#[allow(clippy::too_many_arguments)]
#[php_function]
pub fn chronos_request_start(
    traceparent: String,
    tracestate: String,
    baggage: String,
    session_id: String,
    dst_directive: String,
    http_method: String,
    route_pattern: String,
    service_name: String,
) {
    if REQUEST_CONFIG.with(|c| c.borrow().is_some()) {
        enrich_request(
            &tracestate,
            &baggage,
            &session_id,
            &dst_directive,
            &http_method,
            &route_pattern,
            &service_name,
        );
        return;
    }
    start_request(
        &traceparent,
        &tracestate,
        &baggage,
        &session_id,
        &dst_directive,
        // No profile directive on this path, and the PHP signature stays as it
        // is: reaching here means RINIT did NOT start the request, which is the
        // CLI/worker case — there are no request headers or cookies to carry a
        // directive. Every web request is armed natively before PHP runs.
        "",
        http_method,
        &route_pattern,
        service_name,
    );
}

/// The one true request-start path, shared by the RINIT hook, the SDK bridge —
/// and, since native messaging observation, the observer itself: a Bunny
/// delivery scope (`observer::begin_messaging_delivery`) opens its
/// message-scoped request through here with `http_method = "QUEUE"`, exactly as
/// `BunnyTelemetry::openMessage` did over the FFI, so a natively-opened consumer
/// request and a bridge-opened one are indistinguishable downstream (job profile
/// rates, envelope service fallback, honored wire `sampled` flag and all).
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_request(
    traceparent: &str,
    tracestate: &str,
    baggage: &str,
    session_id: &str,
    dst_directive: &str,
    profile_directive: &str,
    http_method: String,
    route_pattern: &str,
    service_name: String,
) {
    let tp = if traceparent.is_empty() {
        None
    } else {
        Some(traceparent)
    };
    let sid = if session_id.is_empty() {
        None
    } else {
        Some(session_id)
    };
    let mut context = TraceContext::from_header(tp, sid);
    // The inbound `tracestate` and `baggage`, VERBATIM — never parsed. W3C Trace
    // Context requires a participant to forward tracestate it does not understand,
    // so pass-through IS the implementation; the only processing either header gets
    // is a byte cap, because both are caller-controlled input that would otherwise
    // ride on every outbound call of the request. 4 KiB comfortably clears the
    // spec's own 512-char tracestate guidance while bounding a hostile header.
    context.tracestate = Some(cap(tracestate.trim(), 4096)).filter(|s| !s.is_empty());
    context.baggage = Some(cap(baggage.trim(), 4096)).filter(|s| !s.is_empty());

    let config = CollectorConfig::resolve();
    if !config.enabled {
        return;
    }
    // A collector with no identity envelope can never flush (request_end bails on
    // it), so collecting anything for the request would be pure overhead.
    if config.envelope.is_none() {
        return;
    }

    heartbeat(&config);

    // The profile decision, made BEFORE the trace decision because it can force it.
    //
    //   forced  — the directive carried the shared secret. Always profiles.
    //   rolled  — the rate die for this workload came up. Independent of the
    //             APM rate, so "1% profiling" means 1% of requests rather than
    //             1% of whatever APM already kept.
    //
    // A profile with no sampled trace to hang off is an orphan: the desktop
    // reaches a profile THROUGH its request, so anything that profiles must also
    // trace. Hence the upgrade below rather than an `&& context.sampled` gate.
    let forced_profile =
        config.profiler_enabled && profile_forced(profile_directive, &config.profile_token);
    // Web requests and background jobs get their own rates. Which one applies is
    // read off the method: see `is_background_job`.
    // Captured before `http_method` is moved into thread-local state below.
    let background_job = is_background_job(&http_method);
    let profile_rate = if background_job {
        config.profile_job_rate
    } else {
        config.profile_request_rate
    };
    let profile_this_request = if !config.profiler_enabled {
        false
    } else if forced_profile {
        true
    } else if context.parent_span_id.is_none() {
        head_sample(profile_rate)
    } else {
        // An inbound traceparent already decided whether this trace is kept, and
        // rolling our own die on a child would force-sample one service of a
        // distributed trace against its root's decision. So a child profiles only
        // within a trace that is already sampled — which does mean the effective
        // rate here is the profile rate TIMES the caller's, and that is stated in
        // the install guide rather than silently surprising someone.
        context.sampled && head_sample(profile_rate)
    };

    // Head sampling: only a locally-rooted trace makes its own decision; an inbound
    // traceparent's sampled flag is always honored so traces stay whole across services.
    if context.parent_span_id.is_none() {
        context.sampled = head_sample(config.apm_sample_rate);
    }
    // A profiled request is always a traced one — see above. On a child request
    // this can only be reached by a FORCED profile, which is an explicit human
    // instruction and so is allowed to override the root's sampling decision;
    // the result is a partial distributed trace, which is the honest outcome of
    // asking one service for a profile the caller never asked for.
    if profile_this_request {
        context.sampled = true;
    }

    let envelope = config.envelope.clone();
    // The reported service name is a DISPLAY LABEL; the application id is the
    // identity every query, the catalog and the team scoping already agree on.
    // A bridge with nothing better to offer passes an empty string, so resolve
    // the fallback ONCE here rather than at each use: leaving it empty set the
    // request's name from the envelope at flush but left the profile's `service`
    // label unset, so a service's profiles and its spans disagreed on its name.
    let service_name = if service_name.is_empty() {
        envelope
            .as_ref()
            .map(|e| e.application_id.clone())
            .unwrap_or_default()
    } else {
        service_name
    };
    REQUEST_CONFIG.with(|c| *c.borrow_mut() = Some(config.clone()));
    REQUEST_ENVELOPE.with(|e| *e.borrow_mut() = envelope.clone());
    REQUEST_CONTEXT.with(|c| *c.borrow_mut() = Some(context.clone()));
    REQUEST_START_NS.with(|s| *s.borrow_mut() = monotonic_nanos());
    REQUEST_STARTED_AT.with(|s| {
        *s.borrow_mut() = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string();
    });
    REQUEST_HTTP_METHOD.with(|m| *m.borrow_mut() = http_method);
    REQUEST_SERVICE_NAME.with(|n| *n.borrow_mut() = service_name.clone());

    observer::set_request_context(context.clone());
    log_spool::reset();
    // Same reason as the line above: a thread-local outlives one request inside
    // an FPM worker, and a payload left buffered would be flushed against the
    // NEXT request's envelope and start instant.
    body_spool::reset();
    request_attributes::reset();
    // Deterministic aggregates (ADR 0029) are armed on EVERY request the collector
    // starts, not on the sample verdict and not behind `profiler_enabled`. Tier 1's
    // buffer is O(distinct functions), so "always on with a kill switch" is affordable
    // where the sampler's timer and stack walks are not — and a service that samples 1%
    // of requests still gets exact call counts for 100% of them.
    deterministic::reset_request(config.deterministic);

    // Full HTTP stack capture rides the head-sampling decision for the same reason
    // profiling does: an unsampled request has no span to hang headers off, so
    // reading the superglobals and php://input for it would be pure overhead.
    if config.apm_enabled && context.sampled {
        http_capture::on_request_start(config.http_capture.clone());
    } else {
        http_capture::reset();
    }

    // DST recording: armed by the global flag, or per request by an explicit
    // `record` directive (x-chronos-dst header / chronos_dst cookie).
    let directive_records = dst_directive
        .split(&[';', ','][..])
        .any(|part| matches!(part.trim(), "record" | "record=1" | "record=true"));
    let dst_armed = config.dst_enabled || directive_records;
    if dst_armed {
        dst_spool::activate();
    } else {
        dst_spool::reset();
    }

    // TIER 3 (bounded, redacted, scalar-only argument capture on manifest-allowlisted
    // functions) needs TWO independent gates and this is the second one.
    // `CHRONOS_PHP_PROFILE_ARGS` says the deployment ALLOWS it; this says the REQUEST
    // asked for it — either by carrying the forced-profile secret, or by arming a DST
    // recording that a replay will need argument-level path detail from. A flag alone
    // never turns argument capture on for ordinary traffic, deliberately.
    if forced_profile || dst_armed {
        deterministic::arm_arguments();
    }

    // Profiling runs on its OWN verdict (decided above), not on the APM one. An
    // unprofiled request arms no timer and walks no stacks.
    if profile_this_request {
        if let Some(sampler_config) = sampler::SamplerConfig::resolve() {
            sampler::on_request_start(&context, &sampler_config);
            // Why this profile exists, on the profile itself: a reader looking at
            // a flame graph needs to know whether they are seeing a representative
            // sample or the one request somebody forced, because the two answer
            // completely different questions about the service.
            sampler::set_label("trigger", if forced_profile { "forced" } else { "sampled" });
            // Jobs and web requests are sampled at different rates, so a reader
            // aggregating profiles has to be able to separate the populations —
            // mixing a tenth of the jobs into a percent of the requests would
            // over-weight the jobs by a factor nobody can see on a flame graph.
            if background_job {
                sampler::set_label("workload", "job");
            }
            // Frameworks that resolve their route before dispatch (or a static entry
            // point) can tag now; the rest are tagged at flush from request_end.
            if !route_pattern.is_empty() {
                sampler::set_label("route", route_pattern);
            }
            if !service_name.is_empty() {
                sampler::set_label("service", &service_name);
            }
        }
    }
}

/// Enrich an already-open request with what only the SDK's framework bridge knows.
/// Never touches the trace context's identity (trace/span ids, sampled flag) — the
/// observer has been recording against it since RINIT.
fn enrich_request(
    tracestate: &str,
    baggage: &str,
    session_id: &str,
    dst_directive: &str,
    http_method: &str,
    route_pattern: &str,
    service_name: &str,
) {
    if !session_id.is_empty() {
        REQUEST_CONTEXT.with(|c| {
            if let Some(ctx) = c.borrow_mut().as_mut() {
                if ctx.session_id.is_none() {
                    ctx.session_id = Some(session_id.to_owned());
                }
            }
        });
    }
    // Propagation headers fill in only when the native start saw none (a bridge
    // can decode them from places RINIT cannot see, e.g. a queue-message header).
    // Fill-if-unset like the session id above: the request's identity — and what
    // rides with it — is fixed at start, and the observer may already have
    // forwarded the started values on an outbound call.
    if !tracestate.is_empty() || !baggage.is_empty() {
        let mut filled: Option<(Option<String>, Option<String>)> = None;
        REQUEST_CONTEXT.with(|c| {
            if let Some(ctx) = c.borrow_mut().as_mut() {
                if ctx.tracestate.is_none() {
                    ctx.tracestate =
                        Some(cap(tracestate.trim(), 4096)).filter(|s| !s.is_empty());
                }
                if ctx.baggage.is_none() {
                    ctx.baggage = Some(cap(baggage.trim(), 4096)).filter(|s| !s.is_empty());
                }
                filled = Some((ctx.tracestate.clone(), ctx.baggage.clone()));
            }
        });
        // The observer works off its own CLONE of the context, taken at start —
        // curl injection reads that copy, so the fill has to reach it too.
        if let Some((tracestate, baggage)) = filled {
            observer::set_propagation(tracestate, baggage);
        }
    }
    if !http_method.is_empty() {
        REQUEST_HTTP_METHOD.with(|m| *m.borrow_mut() = http_method.to_owned());
    }
    if !service_name.is_empty() {
        REQUEST_SERVICE_NAME.with(|n| *n.borrow_mut() = service_name.to_owned());
        sampler::set_label("service", service_name);
    }
    if !route_pattern.is_empty() {
        sampler::set_label("route", route_pattern);
    }
    // The bridge may know a DST directive the native start could not see (a
    // framework-decoded cookie, a queue-message header). Activation is one-way for
    // the request; deactivation stays with request_end.
    let directive_records = dst_directive
        .split(&[';', ','][..])
        .any(|part| matches!(part.trim(), "record" | "record=1" | "record=true"));
    if directive_records {
        dst_spool::activate();
    }
}

/// PHP-callable: merge extra attributes onto the request root span that will be
/// written at `chronos_request_end`. Framework bridges use this for facts the
/// .so cannot observe — route action, authenticated user id, view/model counts.
///
/// Last write wins per key. Empty keys/values are ignored. New keys beyond the
/// cap are dropped; overwriting an existing key always lands. No-op when the
/// collector is inert for this request.
#[php_function]
pub fn chronos_set_request_attributes(attributes: std::collections::HashMap<String, String>) {
    if REQUEST_CONFIG.with(|c| c.borrow().is_none()) {
        return;
    }
    request_attributes::merge(attributes);
}

/// PHP-callable: declare the observed application's language/framework/release
/// identity for this request. Called by the SDK's framework bridge right after
/// `chronos_request_start`, because only userland can cheaply read `PHP_VERSION`
/// and the framework's own version constant. Empty strings leave the configured
/// value alone, so a bridge that knows only the framework need not invent a
/// version. No-op when the collector is inert (no envelope for this request).
#[php_function]
pub fn chronos_set_app_metadata(
    language_version: String,
    framework: String,
    framework_version: String,
    app_version: String,
) {
    REQUEST_ENVELOPE.with(|cell| {
        if let Some(envelope) = cell.borrow_mut().as_mut() {
            if !language_version.is_empty() {
                envelope.app_language_version = Some(cap(&language_version, 64));
            }
            if !framework.is_empty() {
                envelope.app_framework = Some(cap(&framework, 64));
            }
            if !framework_version.is_empty() {
                envelope.app_framework_version = Some(cap(&framework_version, 64));
            }
            if !app_version.is_empty() {
                envelope.app_version = Some(cap(&app_version, 64));
            }
        }
    });
}

/// PHP-callable: signal request end, flush all collected data.
///
/// Idempotent: the first call clears the request state, so a second call (e.g. the
/// userland fatal-error shutdown safety net after a normal end) is a no-op. The three
/// optional error arguments attach exception identity to the request root span.
// The arity is the PHP-side calling contract (`NativeExtension::requestEnd` and the
// SDK's fatal-error shutdown net), not a choice this crate gets to make smaller.
#[allow(clippy::too_many_arguments)]
#[php_function]
pub fn chronos_request_end(
    http_status_code: i64,
    route_pattern: String,
    error_type: Option<String>,
    error_message: Option<String>,
    error_stack: Option<String>,
    error_code: Option<String>,
    error_exit_code: Option<i64>,
    error_handled: Option<bool>,
) {
    end_request_full(
        http_status_code,
        route_pattern,
        error_type.unwrap_or_default(),
        error_message.unwrap_or_default(),
        error_stack.unwrap_or_default(),
        // `error.code` is the throwable's own code — a string because PHP allows a
        // non-integer code (PDOException carries SQLSTATE like `42S02`). Distinct from
        // `error.exit_code`, the process exit status, which only a fatal shutdown has.
        error_code.unwrap_or_default(),
        error_exit_code,
        error_handled,
    );
}

/// Close the open request from NATIVE code — the observer's seam for ending a
/// message-scoped request it opened itself (`observer::begin_messaging_delivery`).
///
/// Exists because `chronos_request_end` is the PHP calling contract and the
/// observer is not a PHP caller: it has no exit code, no route-less shutdown
/// net, and its error identity is a `LastThrow` rather than a Throwable — the
/// tuple is `(type, message, stack, handled)`, the four facts a delivery scope
/// can honestly state. `error.code` stays empty (a native close never inspected
/// the throwable object; inventing `0` would read as "captured as zero").
pub(crate) fn end_request(
    http_status_code: i64,
    route_pattern: String,
    error: Option<(String, String, String, bool)>,
) {
    match error {
        Some((error_type, error_message, error_stack, handled)) => end_request_full(
            http_status_code,
            route_pattern,
            error_type,
            error_message,
            error_stack,
            String::new(),
            None,
            Some(handled),
        ),
        None => end_request_full(
            http_status_code,
            route_pattern,
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            None,
            None,
        ),
    }
}

/// The body `chronos_request_end` always had, callable from both the PHP calling
/// contract and the observer's native close. Idempotent exactly as before: the
/// first call takes the request state, so a second is a no-op.
#[allow(clippy::too_many_arguments)]
fn end_request_full(
    http_status_code: i64,
    route_pattern: String,
    mut error_type: String,
    mut error_message: String,
    mut error_stack: String,
    error_code: String,
    error_exit_code: Option<i64>,
    mut error_handled: Option<bool>,
) {
    let config = REQUEST_CONFIG.with(|c| c.borrow_mut().take());
    let config = match config {
        Some(c) => c,
        None => return,
    };
    let envelope = match REQUEST_ENVELOPE.with(|e| e.borrow_mut().take()) {
        Some(e) => e,
        None => return,
    };
    let context = REQUEST_CONTEXT.with(|c| c.borrow_mut().take());
    let request_end_ns = monotonic_nanos();
    let request_start_ns = REQUEST_START_NS.with(|s| *s.borrow());
    let started_at = REQUEST_STARTED_AT.with(|s| s.borrow().clone());
    let http_method = REQUEST_HTTP_METHOD.with(|m| m.borrow().clone());
    let service_name = REQUEST_SERVICE_NAME.with(|n| n.borrow().clone());

    let sampled = context.as_ref().map(|c| c.sampled).unwrap_or(false);
    let extra_attributes = request_attributes::take();

    // A request dying on an exception NOTHING caught, with no framework bridge to
    // say so (plain-PHP scripts, or a framework whose bridge is not installed):
    // the observer's throw hook is the only witness left. A bridge that already
    // reported must win — either through this call's own error arguments or
    // through an `error.*` attribute it merged earlier — so this only ever fills
    // silence, never overwrites a report.
    if error_type.is_empty()
        && !extra_attributes
            .iter()
            .any(|(key, _)| key == "error.type")
    {
        if let Some(throw) = uncaught_exception_at_shutdown() {
            error_type = throw.class;
            error_message = throw.message;
            // The throw site, in the `file:line` shape the bridges' stack argument
            // starts with. The full trace is gone by RSHUTDOWN; the site is not.
            if !throw.file.is_empty() {
                error_stack = format!("{}:{}", throw.file, throw.line);
            }
            // Unhandled by definition: the request died on it.
            error_handled = Some(false);
        }
    }
    let errored = !error_type.is_empty();
    if config.apm_enabled && sampled {
        let mut spans = observer::drain();
        if let Some(ctx) = &context {
            // The request root span is always emitted so observer/userland spans have
            // an in-batch ancestor and the request carries its HTTP identity.
            let mut attributes: Vec<(String, String)> = vec![("span.kind".into(), "server".into())];
            if !http_method.is_empty() {
                // Legacy + current semconv spelling, same value — see the constants'
                // comment in `http_capture.rs` for why both are kept.
                attributes.push(("http.method".into(), http_method.clone()));
                attributes.push((http_capture::REQUEST_METHOD.into(), http_method.clone()));
            }
            if !route_pattern.is_empty() {
                attributes.push(("http.route".into(), route_pattern.clone()));
            }
            if http_status_code > 0 {
                attributes.push(("http.status_code".into(), http_status_code.to_string()));
                attributes.push((
                    http_capture::RESPONSE_STATUS_CODE.into(),
                    http_status_code.to_string(),
                ));
            }
            if errored {
                attributes.push(("error.type".into(), cap(&error_type, 256)));
                if !error_message.is_empty() {
                    attributes.push(("error.message".into(), cap(&error_message, 2048)));
                }
                if !error_stack.is_empty() {
                    attributes.push(("error.stack".into(), cap(&error_stack, 16384)));
                }
                // Emitted even when zero: "code 0" is meaningful triage
                // information, and its absence would read as "not captured".
                attributes.push(("error.code".into(), cap(&error_code, 64)));
                if let Some(exit_code) = error_exit_code {
                    attributes.push(("error.exit_code".into(), exit_code.to_string()));
                }
                // Whether the framework rendered the throwable into a response
                // (handled) or it escaped to the middleware boundary (unhandled).
                if let Some(handled) = error_handled {
                    attributes.push((
                        "error.handled".into(),
                        if handled {
                            "true".into()
                        } else {
                            "false".into()
                        },
                    ));
                }
            }
            // Headers, cookies, query, bodies and the phase timeline, resolved and
            // redacted by the collector. Its keys (`http.request.*`, `http.response.*`,
            // `url.*`, `http.timeline`) are disjoint from the identity attributes set
            // above, so append order carries no precedence question.
            let drained = http_capture::drain(request_end_ns.saturating_sub(request_start_ns));
            attributes.extend(drained.attributes);
            attributes.extend(extra_attributes);
            // Spooled before the span, keyed by the same (trace, span). A reader
            // only asks for a body after seeing the span say `.stored`, so writing
            // the body first is the ordering that cannot show a promise the store
            // has not yet been able to honour.
            if !drained.bodies.is_empty() {
                report_body_flush(body_spool::flush(
                    &envelope,
                    &ctx.trace_id,
                    &ctx.span_id,
                    &started_at,
                    &drained.bodies,
                ));
            }
            // Payloads userland handed over during the request, each keyed by
            // the span that already claims `.stored`. Written HERE, before the
            // span batch below and beside the HTTP bodies, for the same reason:
            // a reader only asks for a payload after seeing a span promise it,
            // so the bytes must never be the later of the two writes.
            let pending = body_spool::drain();
            if !pending.is_empty() {
                report_body_flush(body_spool::flush_pending(&envelope, &pending));
            }
            let root = observer::root_http_span(
                ctx,
                &route_pattern,
                started_at,
                http_status_code,
                request_start_ns,
                request_end_ns,
                if errored { "error" } else { "" },
                attributes,
            );
            spans.insert(0, root);
        }
        if !spans.is_empty() {
            let name = if service_name.is_empty() {
                envelope.application_id.clone()
            } else {
                service_name
            };
            // Byte-aware chunking: ingest rejects bodies over 1 MiB, and span
            // sizes vary wildly (a bare userland span is ~0.5KB; a SQL span can
            // carry a 16KiB statement). Split on estimated serialised size with
            // generous headroom rather than a fixed span count.
            const TARGET_CHUNK_BYTES: usize = 512 * 1024;
            const PER_SPAN_OVERHEAD: usize = 384;
            let mut chunk: Vec<observer::NativeSpan> = Vec::new();
            let mut chunk_bytes = 0usize;
            for span in spans {
                let span_bytes = PER_SPAN_OVERHEAD
                    + span.name.len()
                    + span
                        .attributes
                        .iter()
                        .map(|(k, v)| k.len() + v.len() + 8)
                        .sum::<usize>();
                if !chunk.is_empty() && chunk_bytes + span_bytes > TARGET_CHUNK_BYTES {
                    let body = spool::serialise_batch(&envelope, &chunk, &name);
                    let _ = spool::write_atomic(&envelope.tenant_spool_directory(), &body);
                    chunk.clear();
                    chunk_bytes = 0;
                }
                chunk_bytes += span_bytes;
                chunk.push(span);
            }
            if !chunk.is_empty() {
                let body = spool::serialise_batch(&envelope, &chunk, &name);
                let _ = spool::write_atomic(&envelope.tenant_spool_directory(), &body);
            }
        }
    }

    // Userland tags (`Chronos\profile_tag`, e.g. Laravel's `action`) live in the
    // sampler's label map, and the profiler's flush below TAKES that map. Snapshot
    // it first so the counted profile can carry the same tags: without this, a
    // profile opened by `action` filtered the counted read to a tag counted rows
    // never had, and the Functions panel reported "nothing counted" for a signal
    // that was collecting normally.
    let userland_tags = sampler::labels_snapshot();

    if config.profiler_enabled {
        // Tags are resolved HERE, at flush, not at capture: a framework does not know
        // its route until routing has run, which is long after the first samples exist.
        // `route` is the dimension that makes an application-wide profile usable per
        // endpoint, and it is only knowable now.
        if !route_pattern.is_empty() {
            sampler::set_label("route", &route_pattern);
        }
        let method = REQUEST_HTTP_METHOD.with(|m| m.borrow().clone());
        if !method.is_empty() {
            sampler::set_label("http.method", &method);
        }
        if error_type.is_empty() {
            sampler::set_label(
                "outcome",
                if http_status_code >= 500 {
                    "error"
                } else {
                    "ok"
                },
            );
        } else {
            sampler::set_label("outcome", "error");
        }
        // The revision serving this request, under the SAME keys the span attributes
        // use (`spool::attributes`). Spelling them identically across the two signals
        // is what lets a reader compare two commits on the flame graph and then find
        // the same two commits on a trace: one vocabulary, two signals.
        //
        // Resolved once per process (`vcs::revision` memoises), so stamping it per
        // request costs two map inserts, not a filesystem walk.
        let revision = vcs::revision(&envelope.spool_directory);
        if !revision.commit.is_empty() {
            sampler::set_label("app.commit", &revision.commit);
        }
        if !revision.branch.is_empty() {
            sampler::set_label("app.branch", &revision.branch);
        }
        let labels = sampler::take_labels();
        let samples = sampler::on_request_end();
        // Byte-chunked inside `profile_spool::flush` (`spool_common::write_chunked`): a
        // long request at a high sample rate can buffer thousands of deep stacks, and a
        // fixed per-file sample count has no way to bound bytes when a bare stack is
        // ~200 bytes but a 127-frame one is tens of KiB — exactly how mercury's profile
        // spool was dead-lettering whole flushes at the old ingest cap.
        let _ = profile_spool::flush(&envelope, &samples, &labels);
    }

    // DETERMINISTIC PROFILE AGGREGATES (ADR 0029) — the counted sibling of the block
    // above, and gated differently on purpose. Not on `sampled`, because Tier 1 is
    // always-on and must not inherit the profiler's rate verdict; not on
    // `profiler_enabled`, because the two signals share a name and nothing else. The
    // only switch is the tier's own kill switch, already resolved into
    // `config.deterministic`.
    if config.deterministic.aggregates {
        let window = deterministic::drain_window();
        // An empty window writes nothing (see `deterministic_spool::flush_with_budget`):
        // a document claiming coverage of a request in which nothing was observed is a
        // different fact from "not covered", and a reader cannot tell them apart.
        if !window.is_empty() {
            // Labels are resolved HERE, at flush, for the same reason the sampler's are:
            // a framework does not know its route until routing has run, long after the
            // first calls were counted. Built independently of `sampler::take_labels`
            // (which the profiler block already consumed, and which does not exist at
            // all when the profiler is off) but spelled with the SAME keys, so a reader
            // can pivot a counted read and a sampled one on one vocabulary.
            //
            // Seeded from the userland tags so `action` (and anything else the
            // application tagged) is pivotable on BOTH signals — the keys below
            // are inserted after, and win, because a route resolved here is
            // authoritative over one a caller tagged by hand.
            let mut labels: std::collections::BTreeMap<String, String> = userland_tags;
            if !route_pattern.is_empty() {
                labels.insert("route".to_owned(), route_pattern.clone());
            }
            if !http_method.is_empty() {
                labels.insert("http.method".to_owned(), http_method.clone());
            }
            labels.insert(
                "outcome".to_owned(),
                if errored || http_status_code >= 500 {
                    "error".to_owned()
                } else {
                    "ok".to_owned()
                },
            );
            // Memoised per process by `vcs::revision`, so this is two map inserts rather
            // than a filesystem walk — and it is what lets a reader compare exact call
            // counts across two commits.
            let revision = vcs::revision(&envelope.spool_directory);
            if !revision.commit.is_empty() {
                labels.insert("app.commit".to_owned(), revision.commit.clone());
            }
            if !revision.branch.is_empty() {
                labels.insert("app.branch".to_owned(), revision.branch.clone());
            }
            let interval = deterministic_spool::Window {
                from: REQUEST_STARTED_AT.with(|started| started.borrow().clone()),
                to: chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.6fZ")
                    .to_string(),
                // Off the MONOTONIC clock, not by subtracting the two wall-clock strings
                // above: those can straddle an NTP correction, and a negative window
                // would be indistinguishable from a very large one.
                wall_nanoseconds: request_end_ns.saturating_sub(request_start_ns),
            };
            let _ = deterministic_spool::flush(
                &envelope,
                &window,
                &interval,
                context.as_ref(),
                &labels,
            );
        }
    }
    // Disarm for the gap between requests. A call observed outside a request must record
    // nothing, and the next request re-arms with its own freshly resolved config.
    deterministic::reset_request(deterministic::DeterministicConfig::off());

    if config.logs_enabled {
        let logs = log_spool::drain();
        let _ = log_spool::flush(&envelope, &logs);
    }

    if dst_spool::is_active() {
        if call_path::was_truncated() {
            dst_spool::record(
                dst_spool::DstEventKind::Custom("call_path_truncated".to_owned()),
                vec![
                    (
                        "retained".to_owned(),
                        call_path::retained_count().to_string(),
                    ),
                    ("max".to_owned(), call_path::caps().max_events.to_string()),
                ],
            );
        }
        let events = dst_spool::drain();
        dst_spool::deactivate();
        if let Some(ctx) = &context {
            let _ = dst_spool::flush(&envelope, &events, &ctx.trace_id, ctx.session_id.as_deref());
        }
    }

    observer::clear_request_context();
    http_capture::reset();
}

/// The error identity of a request ending on an UNCAUGHT exception that no
/// framework bridge reported — or `None` when the request did not end that way.
///
/// THE HEURISTIC, clause by clause, because each one guards a real false positive:
///
/// * `observer::last_throw()` recorded a throw this request. The throw hook fires
///   on EVERY throw, caught or not, so on its own this only says "an exception
///   existed at some point" — necessary, nowhere near sufficient.
/// * `error_get_last()` reports an `E_ERROR` fatal whose message starts with
///   `Uncaught `. An uncaught exception is the one thing PHP reports in exactly
///   that shape, and such a fatal HALTS execution on the spot — so a request that
///   ran on past its last throw (the exception was caught) cannot have one, and a
///   request killed by a different fatal (OOM, timeout, E_PARSE) has a message of
///   a different shape. This clause is what lets the check run on every
///   request-end, not just the RSHUTDOWN path: on a normally-completed request it
///   is simply never true.
/// * The fatal's message names the recorded throw's class, tying the fatal to the
///   SPECIFIC throw the hook last saw rather than to "some exception happened and
///   was caught, and then something else fatal occurred" — unreachable in one
///   request (a fatal ends it), but cheap insurance for SDK-managed worker
///   "requests" that share one PHP process lifetime and one `error_get_last` slot.
///
/// `error_get_last` is called as a PHP function (same pattern as the observer's
/// `curl_getinfo`) because the engine keeps the last error in globals ext-php-rs
/// does not expose; userland shutdown functions already run after the fatal, so
/// the executor is still able to answer at RSHUTDOWN.
fn uncaught_exception_at_shutdown() -> Option<observer::LastThrow> {
    let throw = observer::last_throw()?;
    let func = ext_php_rs::zend::Function::try_from_function("error_get_last")?;
    let result = func.try_call(vec![]).ok()?;
    let last = result.array()?;
    // PHP's E_ERROR — the severity an uncaught exception is reported at.
    const E_ERROR: i64 = 1;
    if last.get("type").and_then(ext_php_rs::types::Zval::long) != Some(E_ERROR) {
        return None;
    }
    let message = last
        .get("message")
        .and_then(ext_php_rs::types::Zval::str)
        .unwrap_or("");
    if !message.starts_with("Uncaught ") || !message.contains(throw.class.as_str()) {
        return None;
    }
    Some(throw)
}

/// PHP-callable: append a finished userland span (SpanManager / Doctrine listeners)
/// into the native span batch. Timestamps are the collector's UTC `Y-m-d\TH:i:s.u\Z`.
// The arity is the PHP-side calling contract (`NativeExtension::recordSpan`), not a
// choice this crate gets to make smaller.
#[allow(clippy::too_many_arguments)]
#[php_function]
pub fn chronos_record_span(
    trace_id: String,
    span_id: String,
    parent_span_id: String,
    name: String,
    started_at: String,
    ended_at: String,
    attributes: std::collections::HashMap<String, String>,
    status: Option<String>,
) {
    let status = status
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ok".to_owned());
    let enabled =
        REQUEST_CONFIG.with(|c| c.borrow().as_ref().map(|c| c.apm_enabled).unwrap_or(false));
    if !enabled {
        return;
    }
    observer::record_userland_span(
        trace_id,
        span_id,
        parent_span_id,
        name,
        started_at,
        ended_at,
        status,
        attributes.into_iter().collect(),
    );
}

/// PHP-callable: tag this request's profile samples.
///
/// The language-agnostic seam for the dimension the collector cannot infer. `route` and
/// `http.method` are set automatically, but "which action ran", "which queue job", or
/// "which tenant tier" is framework knowledge — every SDK sets it the same way here,
/// and the tag lands identically on every sample of the request.
///
/// Keys are normalised to `[a-z0-9._-]`, values truncated, and the map is capped, so a
/// caller cannot inflate the engine's label cardinality without bound. No-op when the
/// request is not being profiled.
#[php_function]
pub fn chronos_profile_tag(key: String, value: String) {
    sampler::set_label(&key, &value);
}

/// PHP-callable: capture a log record for batched spool write at request end.
#[php_function]
pub fn chronos_capture_log(
    severity_text: String,
    severity_number: i64,
    body: String,
    attributes: std::collections::HashMap<String, String>,
) {
    // Records buffered here are only ever flushed when logs are on for this
    // request; capturing them otherwise would be pay-and-throw-away.
    let logs_enabled =
        REQUEST_CONFIG.with(|c| c.borrow().as_ref().map(|c| c.logs_enabled).unwrap_or(false));
    if !logs_enabled {
        return;
    }
    let context = REQUEST_CONTEXT.with(|c| c.borrow().clone());
    let (trace_id, span_id) = context.map(|c| (c.trace_id, c.span_id)).unwrap_or_default();

    log_spool::capture(log_spool::LogRecord {
        severity_text,
        severity_number: severity_number as i32,
        body,
        trace_id,
        span_id,
        observed_at: chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string(),
        attributes: attributes.into_iter().collect(),
    });
}

/// PHP-callable: announce that a queued job has STARTED, so it is visible while
/// it runs rather than only once it has finished.
///
/// Every other signal leaves at RSHUTDOWN, which is why a running job is
/// invisible: the span index only receives finished spans. This writes a small
/// marker immediately, carrying the trace and span ids of the job-scoped request
/// — so the job's own root span closes the in-flight row by identity when it
/// eventually lands. See `job_spool` for the deadline-not-heartbeat reasoning.
///
/// `timeout_seconds` is the framework's own job timeout, or 0 when it reports
/// none; a job cannot legitimately outlive it, so it becomes the deadline after
/// which a worker that died mid-job may be presumed dead. No-op when the
/// collector is inert for this request, and a failed write is swallowed: a job
/// must not fail because telemetry about it could not be spooled.
#[php_function]
pub fn chronos_job_started(
    name: String,
    timeout_seconds: i64,
    facts: std::collections::HashMap<String, String>,
) {
    if REQUEST_CONFIG.with(|c| c.borrow().is_none()) {
        return;
    }
    let Some((trace_id, span_id)) =
        REQUEST_CONTEXT.with(|c| c.borrow().as_ref().map(|c| (c.trace_id.clone(), c.span_id.clone())))
    else {
        return;
    };
    REQUEST_ENVELOPE.with(|cell| {
        if let Some(envelope) = cell.borrow().as_ref() {
            let _ = job_spool::flush(
                envelope,
                &job_spool::JobRun {
                    trace_id,
                    span_id,
                    name,
                    timeout_seconds: u64::try_from(timeout_seconds).ok().filter(|s| *s > 0),
                    facts,
                },
            );
        }
    });
}

/// PHP-callable: the userland SDK announces its own richer instrumentation for a
/// data-access kind ("sql" | "cache" | "messaging"), and the native observer
/// stops emitting its fallback capture for this request — userland spans carry
/// facts the .so cannot see (host/db/bound params; a publish's DTO class), and
/// double capture splits the service map.
///
/// `"messaging"` governs the PUBLISH side only: `BunnyTelemetry::publish`
/// declares it before calling `$channel->publish`, so the native
/// `Bunny\AbstractClient::publish` observation stands down — no duplicate span,
/// and no injection either (the bridge already put its reserved span id on the
/// wire; even without suppression the native caller-wins hash-add could not
/// clobber it, so suppression removes only the duplicate SPAN). Consume-side
/// ownership deliberately CANNOT be a per-request kind here: the native
/// delivery scope opens at `Bunny\Channel::onBodyComplete`, strictly before any
/// userland code of the delivery could call this — so consume ownership is the
/// process flag `CHRONOS_PHP_MESSAGING_AUTO=0` plus the observer's
/// already-active guard (a userland-opened request makes the native scope pass
/// through), and the userland wrapper's own `NativeExtension::active()` check
/// is the dedupe in the other direction.
#[php_function]
pub fn chronos_suppress_native(kind: String) {
    observer::suppress_native(&kind);
}

/// PHP-callable: register a userland function/method into the trace allowlist. The
/// userland SDK's `Chronos\trace_method` bridges the application's instrumentation
/// manifest here so the Zend observer emits spans for exactly those calls (framework
/// call trees are otherwise the profiler's job, not the trace waterfall's).
///
/// Names use the observer's qualified format: `Class::method` or a plain
/// `function_name`. The allowlist is per-PROCESS and only ever grows, so the SDK
/// loads the manifest once per worker.
#[php_function]
pub fn chronos_trace_function(name: String) {
    observer::trace_function(&name);
}

/// PHP-callable: record a DST effect.
#[php_function]
pub fn chronos_record_dst(kind: String, payload: std::collections::HashMap<String, String>) {
    let event_kind = match kind.as_str() {
        "time" => dst_spool::DstEventKind::Time,
        "random" => dst_spool::DstEventKind::Random,
        "database_query" => dst_spool::DstEventKind::DatabaseQuery,
        "database_result" => dst_spool::DstEventKind::DatabaseResult,
        "cache_read" => dst_spool::DstEventKind::CacheRead,
        "cache_write" => dst_spool::DstEventKind::CacheWrite,
        "http_request" => dst_spool::DstEventKind::HttpRequest,
        "http_response" => dst_spool::DstEventKind::HttpResponse,
        "env_read" => dst_spool::DstEventKind::EnvRead,
        "call" => dst_spool::DstEventKind::Call,
        "exception" => dst_spool::DstEventKind::Exception,
        other => dst_spool::DstEventKind::Custom(other.to_owned()),
    };
    dst_spool::record(event_kind, payload.into_iter().collect());
}

/// PHP-callable: arm scalar builtin overrides for replay (time/random/getenv → Effect).
/// Requires userland `chronos_replay_effect_delegate($kind, $selector)` from bootstrap.
#[php_function]
pub fn chronos_replay_arm() {
    replay_hooks::arm();
}

/// PHP-callable: whether a request is currently open in the collector. False when
/// the collector is disabled, mis-configured (no identity envelope), or the process
/// is CLI without `CHRONOS_PHP_CLI_ENABLED`. The SDK bridges gate their per-request
/// work on this so a disabled collector costs the application nothing.
#[php_function]
pub fn chronos_request_active() -> bool {
    REQUEST_CONFIG.with(|c| c.borrow().is_some())
}

/// PHP-callable: whether THIS request's HTTP stack is being captured (enabled,
/// sampled, capture on). The bridge asks before copying a response body across
/// the FFI, so an unsampled request never pays for the copy.
#[php_function]
pub fn chronos_http_capturing() -> bool {
    http_capture::is_active()
}

/// PHP-callable: resolve a Chronos setting through the unified layer
/// (process env > `chronos.*` INI > `.chronos` file), so PHP-side SDK
/// configuration honours exactly the same sources as the native engine.
/// Returns an empty string for an unset setting.
#[php_function]
pub fn chronos_setting(name: String) -> String {
    settings::get(&name).unwrap_or_default()
}

/// PHP-callable: get the outbound traceparent header.
#[php_function]
pub fn chronos_traceparent() -> String {
    REQUEST_CONTEXT
        .with(|c| c.borrow().as_ref().map(|ctx| ctx.header()))
        .unwrap_or_default()
}

/// PHP-callable: every outbound propagation header in one call, as
/// `{traceparent, tracestate, baggage}` with empty strings for whatever the
/// request does not have (including all three when no request is open).
///
/// The userland seam for the PHP bridges (Guzzle middleware, HttpClient
/// decorator, PSR-18): the native observer forwards these on `curl_exec`
/// itself, but an outbound call made through a userland client is invisible to
/// the curl hook until much deeper in the stack, so the bridge asks here and
/// sets the headers on its own request object. `tracestate` is in the answer
/// because W3C Trace Context REQUIRES a participant that forwards traceparent
/// to also forward tracestate it does not understand; `baggage` follows the
/// same forward-as-is contract. Both are handed over VERBATIM as captured.
#[php_function]
pub fn chronos_propagation_headers() -> std::collections::HashMap<String, String> {
    REQUEST_CONTEXT.with(|c| {
        let borrowed = c.borrow();
        let context = borrowed.as_ref();
        std::collections::HashMap::from([
            (
                "traceparent".to_owned(),
                context.map(TraceContext::header).unwrap_or_default(),
            ),
            (
                "tracestate".to_owned(),
                context
                    .and_then(|ctx| ctx.tracestate.clone())
                    .unwrap_or_default(),
            ),
            (
                "baggage".to_owned(),
                context
                    .and_then(|ctx| ctx.baggage.clone())
                    .unwrap_or_default(),
            ),
        ])
    })
}

/// PHP-callable: retrieve the pending traceparent that the observer prepared
/// for a curl call. Returns empty string if none is pending. Called by the
/// PHP SDK's curl integration to inject the header before curl_exec.
#[php_function]
pub fn chronos_pending_traceparent() -> String {
    observer::take_pending_traceparent().unwrap_or_default()
}

/// PHP-callable: generate a child traceparent for an outbound CURL call.
///
/// HTTP-ONLY, and unsafe for anything else. The span id in the middle field is
/// freshly minted here and NOBODY RECORDS IT: no span with that id is ever
/// emitted, so a callee that parents itself to it becomes an orphan sharing only
/// a trace id with its caller. On the curl path that is harmless because it is
/// overwritten — `observer::merge_propagation_headers` strips every existing
/// `traceparent:` off `CURLOPT_HTTPHEADER` and pushes the curl frame's OWN span
/// id, which IS emitted — so the value handed back here is a placeholder the
/// observer replaces.
///
/// Messaging propagation must NOT use this. A message crosses a process
/// boundary, there is no later hook to correct the header, and the consumer's
/// root span keeps the phantom as its parent forever — which is precisely why
/// publish and consume shared a trace but never a tree. The PHP SDK reserves a
/// real publish span id instead (`Service\SpanManager::reserve()` and
/// `Dto\SpanReservation`), puts THAT on the wire, and then records the publish
/// span under the same id. Kept unchanged and still registered because the curl
/// bridges depend on its exact behaviour.
#[php_function]
pub fn chronos_child_traceparent() -> String {
    REQUEST_CONTEXT.with(|c| {
        c.borrow()
            .as_ref()
            .map(|ctx| {
                let child_span_id = context::hex_bytes(8);
                format!(
                    "00-{}-{}-{}",
                    ctx.trace_id,
                    child_span_id,
                    if ctx.sampled { "01" } else { "00" }
                )
            })
            .unwrap_or_default()
    })
}

/// PHP-callable: hand the collector the response body the framework is about to
/// send, plus its content type.
///
/// The native side can read every part of the HTTP stack except this one: by the
/// time `chronos_request_end` runs, the body has gone to the SAPI and PHP kept no
/// copy. A bridge that holds a Response object (Symfony `HttpKernel`, Laravel
/// middleware, the symfony1 filter) calls this instead, which is both exact and
/// free — no output buffer, so streaming and `X-Sendfile` responses are untouched.
///
/// The value beats whatever the optional output-buffer fallback collected. The
/// bridge also supplies the response HEADERS, for a subtler reason: it calls
/// request-end before the framework flushes, so `headers_list()` is still empty
/// natively at that instant.
#[php_function]
pub fn chronos_set_http_response_body(
    body: String,
    content_type: Option<String>,
    headers: Option<std::collections::HashMap<String, String>>,
) {
    http_capture::set_response(
        body,
        content_type.unwrap_or_default(),
        headers
            .map(|map| map.into_iter().collect())
            .unwrap_or_default(),
    );
}

/// PHP-callable: keep a whole payload beside the span that previews it.
///
/// The messaging counterpart of what `http_capture` does for an HTTP exchange,
/// and it reuses that store rather than inventing one: the same
/// `chronos.tracing.span-body.v1` documents, the same `(trace, span, side)` key,
/// the same chunking, so the desktop's existing "load the rest" path reads a
/// message payload with no new read plumbing. `side` is `message` — one value,
/// not `publish`/`consume`, because a publish span and a consume span are
/// different span ids in the same trace and the direction is already on the span
/// as `messaging.operation`.
///
/// Empty `trace_id`/`span_id` mean "this request's root span", which is what the
/// consume side wants: its preview rides the request-attribute bag and therefore
/// lands on the root, so keying the blob anywhere else would break the one
/// invariant the store rests on — the span that promises the payload is the span
/// that owns it. The publish side passes its own ids, because a publish span is
/// a child the PHP SDK minted.
///
/// `encoding` is the transfer encoding of `body` (`base64` for a payload that is
/// not valid UTF-8, empty for text). It is NOT stored: the span's
/// `messaging.message.body.encoding` attribute is the single authority, and a
/// second copy on the wire document would be a second thing to disagree. It is
/// still read here, to REFUSE an encoding this pipeline has no reader for — a
/// stored payload nobody can decode is worse than no payload at all.
///
/// Returns whether the payload was taken. The caller stamps
/// `messaging.message.body.stored` only on `true`, so a marker can never
/// out-promise the store: false means the request is inert, APM is off, the side
/// is not one this entry point serves, the body is empty, the encoding is
/// unreadable, or the per-request buffer ([`body_spool`]) is full.
#[php_function]
pub fn chronos_store_span_body(
    trace_id: String,
    span_id: String,
    side: String,
    content_type: String,
    body: String,
    encoding: String,
) -> bool {
    // Checked before anything else touches the payload, in the order that costs
    // least: two string compares, then an emptiness test, then the thread-locals.
    if side != "message" {
        return false;
    }
    store_message_body(trace_id, span_id, content_type, body, encoding)
}

/// The store half of [`chronos_store_span_body`], shared with the observer's own
/// messaging capture (`observer` stores a publish payload keyed by the publish
/// frame's ids, and a consume payload keyed by the request root via empty ids)
/// so the FFI entry point and the native path apply IDENTICAL gates — apm on,
/// context open, sampled, non-empty, decodable encoding — and a `.stored`
/// marker means one thing however the payload arrived.
pub(crate) fn store_message_body(
    trace_id: String,
    span_id: String,
    content_type: String,
    body: String,
    encoding: String,
) -> bool {
    if encoding != "base64" && !encoding.is_empty() {
        return false;
    }
    if body.is_empty() {
        return false;
    }
    // `apm_enabled` AND a sampled context, because the promise is only worth
    // keeping if the span arrives: an unsampled request emits no spans at all
    // (see `chronos_request_end`), so a payload stored for one would be a row no
    // span ever points at.
    let apm = REQUEST_CONFIG.with(|c| {
        c.borrow()
            .as_ref()
            .map(|config| config.apm_enabled)
            .unwrap_or(false)
    });
    if !apm {
        return false;
    }
    let Some((request_trace, request_span, sampled)) = REQUEST_CONTEXT.with(|c| {
        c.borrow()
            .as_ref()
            .map(|ctx| (ctx.trace_id.clone(), ctx.span_id.clone(), ctx.sampled))
    }) else {
        return false;
    };
    if !sampled {
        return false;
    }

    body_spool::capture(body_spool::PendingBody {
        trace_id: if trace_id.is_empty() {
            request_trace
        } else {
            trace_id
        },
        span_id: if span_id.is_empty() {
            request_span
        } else {
            span_id
        },
        // The REQUEST's start instant, not "now": it is what keeps every chunk of
        // one body in the same Timescale chunk as the span that owns it, and what
        // makes the row's identity derived rather than assigned — which is what
        // makes a redelivered chunk idempotent without a dedupe table.
        observed_at: REQUEST_STARTED_AT.with(|started| started.borrow().clone()),
        body: http_capture::StoredBody {
            // A 'static literal, matching `StoredBody::side`'s type — the closed
            // vocabulary is closed on purpose, and `message` is now one of three.
            side: "message",
            content_type,
            bytes: body,
        },
    })
}

/// PHP-callable: mark the start of a named request phase for the Timeline tab.
///
/// The mark names the phase that is BEGINNING, so a bridge calls
/// `chronos_mark_phase('controller')` as it dispatches. Everything before the first
/// mark is `bootstrap` and everything after the last runs to the end of the
/// request, which means a bridge only has to know its own handful of boundaries —
/// it never has to close a phase it did not open.
///
/// Framework knowledge, so it lives in the bridge rather than the engine: only the
/// framework knows when routing ended and dispatch began. No-op when the request
/// is not being captured.
#[php_function]
pub fn chronos_mark_phase(name: String) {
    let started = REQUEST_START_NS.with(|s| *s.borrow());
    if started == 0 {
        return;
    }
    http_capture::mark_phase(&name, monotonic_nanos().saturating_sub(started));
}

#[php_module]
#[php(startup = "startup")]
pub fn module(module: ModuleBuilder) -> ModuleBuilder {
    module
        // The registered module name is what `extension_loaded()` answers to, and it
        // must match the file everyone installs as `chronos.so` — not the crate name.
        .name("chronos")
        .request_startup_function(request_startup)
        .request_shutdown_function(request_shutdown)
        .function(wrap_function!(chronos_request_start))
        .function(wrap_function!(chronos_set_app_metadata))
        .function(wrap_function!(chronos_set_request_attributes))
        .function(wrap_function!(chronos_request_end))
        .function(wrap_function!(chronos_record_span))
        .function(wrap_function!(chronos_capture_log))
        .function(wrap_function!(chronos_job_started))
        .function(wrap_function!(chronos_profile_tag))
        .function(wrap_function!(chronos_suppress_native))
        .function(wrap_function!(chronos_trace_function))
        .function(wrap_function!(chronos_record_dst))
        .function(wrap_function!(chronos_replay_arm))
        .function(wrap_function!(chronos_request_active))
        .function(wrap_function!(chronos_http_capturing))
        .function(wrap_function!(chronos_setting))
        .function(wrap_function!(chronos_traceparent))
        .function(wrap_function!(chronos_propagation_headers))
        .function(wrap_function!(chronos_pending_traceparent))
        .function(wrap_function!(chronos_child_traceparent))
        .function(wrap_function!(chronos_set_http_response_body))
        .function(wrap_function!(chronos_store_span_body))
        .function(wrap_function!(chronos_mark_phase))
}

fn cap(value: &str, max: usize) -> String {
    spool_common::cap(value, max)
}

/// Whether a body flush that failed has already been announced by THIS process.
static BODY_FLUSH_ANNOUNCED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Report a failed body flush — once per process, never per call.
///
/// `spool_log::report_failure` already announces the write itself, and that is
/// the line an operator acts on. This one exists because a body failure has a
/// consequence the generic message cannot state: the span it belongs to still
/// went out carrying `.stored`, so the reader will be shown a promise and then a
/// 404. Latched separately from the spool's own latches so a request-path span
/// failure cannot consume the body path's only line.
fn report_body_flush(result: std::io::Result<()>) {
    if let Err(error) = result {
        if !BODY_FLUSH_ANNOUNCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "[chronos-ext] a stored body could not be spooled ({error}): spans in this \
                 process may claim a payload that never arrives. Reported once per process."
            );
        }
    }
}

fn monotonic_nanos() -> u128 {
    use std::time::Instant;
    thread_local! { static ORIGIN: Instant = Instant::now(); }
    ORIGIN.with(|origin| origin.elapsed().as_nanos())
}

#[cfg(test)]
mod directive_tests {
    use super::{constant_time_eq, cookie_value, profile_forced};

    #[test]
    fn a_matching_token_arms_a_forced_profile() {
        assert!(profile_forced("s3cret", "s3cret"));
        // The `key=value` spelling, so one cookie can carry several directives.
        assert!(profile_forced("profile=s3cret", "s3cret"));
        assert!(profile_forced("record; profile=s3cret", "s3cret"));
    }

    #[test]
    fn no_configured_token_means_the_directive_does_nothing() {
        // The default posture. Without this, enabling the profiler would silently
        // hand every caller on the internet a switch for the expensive path.
        assert!(!profile_forced("1", ""));
        assert!(!profile_forced("anything", ""));
    }

    #[test]
    fn a_wrong_or_absent_token_does_not_arm_it() {
        assert!(!profile_forced("", "s3cret"));
        assert!(!profile_forced("1", "s3cret"));
        assert!(!profile_forced("s3cre", "s3cret"));
        assert!(!profile_forced("s3crett", "s3cret"));
        assert!(!profile_forced("S3CRET", "s3cret"));
    }

    #[test]
    fn the_comparison_does_not_stop_at_the_first_wrong_byte() {
        // A guess sharing a long prefix must be no cheaper to reject than one
        // that differs immediately — that difference is the timing oracle.
        assert!(!constant_time_eq(b"s3cre_", b"s3cret"));
        assert!(!constant_time_eq(b"______", b"s3cret"));
        assert!(constant_time_eq(b"s3cret", b"s3cret"));
        // An empty secret can never match, including against an empty offer.
        assert!(!constant_time_eq(b"", b""));
    }

    #[test]
    fn the_profile_cookie_is_read_out_of_a_shared_cookie_header() {
        let raw = "session=abc; chronos_profile=s3cret; theme=dark";
        assert_eq!(cookie_value(raw, "chronos_profile"), "s3cret");
        assert!(profile_forced(
            &cookie_value(raw, "chronos_profile"),
            "s3cret"
        ));
    }
}
