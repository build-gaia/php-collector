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

use ext_php_rs::prelude::*;

pub mod config;
pub mod context;
pub mod call_path;
pub mod dst_spool;
pub mod http_capture;
pub mod log_spool;
pub mod metrics_spool;
pub mod observer;
pub mod profile_spool;
pub mod request_attributes;
pub mod replay_hooks;
pub mod sampler;
pub mod settings;
pub mod spool;
pub mod spool_common;
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
    let status = i64::from(ext_php_rs::zend::SapiGlobals::get().sapi_headers().http_response_code);
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
    let session_id = http_capture::lookup(&server, "HTTP_X_CHRONOS_SESSION_ID").to_owned();
    let dst_directive = {
        let header = http_capture::lookup(&server, "HTTP_X_CHRONOS_DST");
        if header.is_empty() {
            cookie_value(http_capture::lookup(&server, "HTTP_COOKIE"), "chronos_dst")
        } else {
            header.to_owned()
        }
    };

    start_request(&traceparent, &session_id, &dst_directive, method, "", String::new());
}

/// Extract one cookie's value from a raw `Cookie:` header line.
fn cookie_value(raw: &str, name: &str) -> String {
    raw.split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_owned())
        .unwrap_or_default()
}

/// Head-sampling decision for a locally-rooted trace: `bps` basis points out of 10_000.
fn head_sample(bps: u32) -> bool {
    if bps >= 10_000 {
        return true;
    }
    if bps == 0 {
        return false;
    }
    rand::thread_rng().gen_range(0..10_000u32) < bps
}

fn heartbeat(config: &CollectorConfig) {
    if HEARTBEAT_SENT.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let summary = format!(
        "chronos-collector active: apm={} sample_bps={} logs={} profiler={} dst={} metrics={}",
        config.apm_enabled,
        config.apm_sample_rate_bps,
        config.logs_enabled,
        config.profiler_enabled,
        config.dst_enabled,
        config.runtime_metrics_enabled,
    );
    // Lands in the SAPI error log (docker logs) even when log shipping is off.
    eprintln!("[chronos-ext] {summary}");
    if config.logs_enabled {
        log_spool::capture(log_spool::LogRecord {
            severity_text: "INFO".into(),
            severity_number: 9,
            body: summary,
            trace_id: String::new(),
            span_id: String::new(),
            observed_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
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
#[php_function]
pub fn chronos_request_start(
    traceparent: String,
    _tracestate: String,
    _baggage: String,
    session_id: String,
    dst_directive: String,
    http_method: String,
    route_pattern: String,
    service_name: String,
) {
    if REQUEST_CONFIG.with(|c| c.borrow().is_some()) {
        enrich_request(&session_id, &dst_directive, &http_method, &route_pattern, &service_name);
        return;
    }
    start_request(
        &traceparent,
        &session_id,
        &dst_directive,
        http_method,
        &route_pattern,
        service_name,
    );
}

/// The one true request-start path, shared by the RINIT hook and the SDK bridge.
fn start_request(
    traceparent: &str,
    session_id: &str,
    dst_directive: &str,
    http_method: String,
    route_pattern: &str,
    service_name: String,
) {
    let tp = if traceparent.is_empty() { None } else { Some(traceparent) };
    let sid = if session_id.is_empty() { None } else { Some(session_id) };
    let mut context = TraceContext::from_header(tp, sid);

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

    // Head sampling: only a locally-rooted trace makes its own decision; an inbound
    // traceparent's sampled flag is always honored so traces stay whole across services.
    if context.parent_span_id.is_none() {
        context.sampled = head_sample(config.apm_sample_rate_bps);
    }

    let envelope = config.envelope.clone();
    REQUEST_CONFIG.with(|c| *c.borrow_mut() = Some(config.clone()));
    REQUEST_ENVELOPE.with(|e| *e.borrow_mut() = envelope.clone());
    REQUEST_CONTEXT.with(|c| *c.borrow_mut() = Some(context.clone()));
    REQUEST_START_NS.with(|s| *s.borrow_mut() = monotonic_nanos());
    REQUEST_STARTED_AT.with(|s| {
        *s.borrow_mut() = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    });
    REQUEST_HTTP_METHOD.with(|m| *m.borrow_mut() = http_method);
    REQUEST_SERVICE_NAME.with(|n| *n.borrow_mut() = service_name.clone());

    observer::set_request_context(context.clone());
    log_spool::reset();
    request_attributes::reset();

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
    if config.dst_enabled || directive_records {
        dst_spool::activate();
    } else {
        dst_spool::reset();
    }

    // Profiling rides the APM head-sampling decision: `context.sampled` is the
    // `CHRONOS_PHP_APM_SAMPLE_RATE` verdict (or an inbound traceparent's), so a profiled
    // request is always one that also has a trace, and an unsampled request arms no
    // timer and walks no stacks.
    if config.profiler_enabled && context.sampled {
        if let Some(sampler_config) = sampler::SamplerConfig::resolve() {
            sampler::on_request_start(&context, &sampler_config);
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
    let error_type = error_type.unwrap_or_default();
    let error_message = error_message.unwrap_or_default();
    let error_stack = error_stack.unwrap_or_default();
    // `error.code` is the throwable's own code — a string because PHP allows a
    // non-integer code (PDOException carries SQLSTATE like `42S02`). Distinct from
    // `error.exit_code`, the process exit status, which only a fatal shutdown has.
    let error_code = error_code.unwrap_or_default();
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
    let errored = !error_type.is_empty();

    let sampled = context.as_ref().map(|c| c.sampled).unwrap_or(false);
    let extra_attributes = request_attributes::take();
    if config.apm_enabled && sampled {
        let mut spans = observer::drain();
        if let Some(ctx) = &context {
            // The request root span is always emitted so observer/userland spans have
            // an in-batch ancestor and the request carries its HTTP identity.
            let mut attributes: Vec<(String, String)> = vec![
                ("span.kind".into(), "server".into()),
            ];
            if !http_method.is_empty() {
                attributes.push(("http.method".into(), http_method.clone()));
            }
            if !route_pattern.is_empty() {
                attributes.push(("http.route".into(), route_pattern.clone()));
            }
            if http_status_code > 0 {
                attributes.push(("http.status_code".into(), http_status_code.to_string()));
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
                        if handled { "true".into() } else { "false".into() },
                    ));
                }
            }
            // Headers, cookies, query, bodies and the phase timeline, resolved and
            // redacted by the collector. Its keys (`http.request.*`, `http.response.*`,
            // `url.*`, `http.timeline`) are disjoint from the identity attributes set
            // above, so append order carries no precedence question.
            attributes.extend(http_capture::drain(
                request_end_ns.saturating_sub(request_start_ns),
            ));
            attributes.extend(extra_attributes);
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
                    let _ = spool::write_atomic(&envelope.spool_directory, &body);
                    chunk.clear();
                    chunk_bytes = 0;
                }
                chunk_bytes += span_bytes;
                chunk.push(span);
            }
            if !chunk.is_empty() {
                let body = spool::serialise_batch(&envelope, &chunk, &name);
                let _ = spool::write_atomic(&envelope.spool_directory, &body);
            }
        }
    }

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
            sampler::set_label("outcome", if http_status_code >= 500 { "error" } else { "ok" });
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

    if config.logs_enabled {
        let logs = log_spool::drain();
        let _ = log_spool::flush(&envelope, &logs);
    }

    if dst_spool::is_active() {
        if call_path::was_truncated() {
            dst_spool::record(
                dst_spool::DstEventKind::Custom("call_path_truncated".to_owned()),
                vec![
                    ("retained".to_owned(), call_path::retained_count().to_string()),
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

    if config.runtime_metrics_enabled {
        if let Some(ctx) = &context {
            let metrics = metrics_spool::RequestMetrics {
                trace_id: ctx.trace_id.clone(),
                span_id: ctx.span_id.clone(),
                http_route: route_pattern,
                http_method,
                http_status_code: http_status_code as u16,
                request_start_ns,
                request_end_ns,
            };
            let _ = metrics_spool::flush(&envelope, &metrics, envelope.app_version.as_deref());
        }
    }

    observer::clear_request_context();
    http_capture::reset();
}

/// PHP-callable: append a finished userland span (SpanManager / Doctrine listeners)
/// into the native span batch. Timestamps are the collector's UTC `Y-m-d\TH:i:s.u\Z`.
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
    let status = status.filter(|s| !s.is_empty()).unwrap_or_else(|| "ok".to_owned());
    let enabled = REQUEST_CONFIG.with(|c| c.borrow().as_ref().map(|c| c.apm_enabled).unwrap_or(false));
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
    let (trace_id, span_id) = context
        .map(|c| (c.trace_id, c.span_id))
        .unwrap_or_default();

    log_spool::capture(log_spool::LogRecord {
        severity_text,
        severity_number: severity_number as i32,
        body,
        trace_id,
        span_id,
        observed_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        attributes: attributes.into_iter().collect(),
    });
}

/// PHP-callable: the userland SDK announces its own richer instrumentation for a
/// data-access kind ("sql" | "cache"), and the native observer stops emitting its
/// fallback I/O spans for this request — userland spans carry host/db/bound
/// params the .so cannot see, and double capture splits the service map.
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

/// PHP-callable: retrieve the pending traceparent that the observer prepared
/// for a curl call. Returns empty string if none is pending. Called by the
/// PHP SDK's curl integration to inject the header before curl_exec.
#[php_function]
pub fn chronos_pending_traceparent() -> String {
    observer::take_pending_traceparent().unwrap_or_default()
}

/// PHP-callable: generate a child traceparent for manual outbound propagation.
/// Returns a W3C traceparent string with a fresh span_id linked to the current trace.
#[php_function]
pub fn chronos_child_traceparent() -> String {
    REQUEST_CONTEXT.with(|c| {
        c.borrow().as_ref().map(|ctx| {
            let child_span_id = context::hex_bytes(8);
            format!(
                "00-{}-{}-{}",
                ctx.trace_id,
                child_span_id,
                if ctx.sampled { "01" } else { "00" }
            )
        }).unwrap_or_default()
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
        headers.map(|map| map.into_iter().collect()).unwrap_or_default(),
    );
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
        .function(wrap_function!(chronos_profile_tag))
        .function(wrap_function!(chronos_suppress_native))
        .function(wrap_function!(chronos_trace_function))
        .function(wrap_function!(chronos_record_dst))
        .function(wrap_function!(chronos_replay_arm))
        .function(wrap_function!(chronos_request_active))
        .function(wrap_function!(chronos_http_capturing))
        .function(wrap_function!(chronos_setting))
        .function(wrap_function!(chronos_traceparent))
        .function(wrap_function!(chronos_pending_traceparent))
        .function(wrap_function!(chronos_child_traceparent))
        .function(wrap_function!(chronos_set_http_response_body))
        .function(wrap_function!(chronos_mark_phase))
}

fn cap(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn monotonic_nanos() -> u128 {
    use std::time::Instant;
    thread_local! { static ORIGIN: Instant = Instant::now(); }
    ORIGIN.with(|origin| origin.elapsed().as_nanos())
}
