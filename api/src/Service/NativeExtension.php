<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * Bridge between PHP framework hooks and the native Rust extension.
 *
 * Every method is fail-open: if the extension is not loaded or a function is
 * unavailable, the call is silently ignored. Framework hooks call these methods
 * instead of the heavy PHP service classes (LocalSpanRecorder, DstRecorder,
 * RuntimeMetricsEmitter, etc.) — all that work now lives in the .so.
 */
final class NativeExtension
{
    private static ?bool $loaded = null;

    private static ?bool $enabled = null;

    /** Resolved once per process: the setting read is an FFI call, and the answer
     * cannot change inside a process. */
    private static ?int $bodyCeiling = null;

    /** Resolved once per process, same reason as $bodyCeiling: the setting read is
     * an FFI call, and a payload-capture decision cannot change mid-process. */
    private static ?bool $messagingCapture = null;

    /** Resolved once per process, for the same reason. */
    private static ?int $messagingCeiling = null;

    /** Once-per-process guard for the instrumentation-manifest load (the native
     * trace allowlist is per-process and only ever grows, so once is enough). */
    private static bool $manifestLoaded = false;

    /** Whether the fatal-error net is already registered for this PROCESS. Only
     * consulted under a one-request-per-process SAPI — see armShutdownNet. */
    private static bool $shutdownNetArmed = false;

    public static function loaded(): bool
    {
        // The module registers as `chronos` (matching the chronos.so filename);
        // `chronos-ext` was the crate-derived name older builds registered under.
        return self::$loaded ??= (extension_loaded('chronos') || extension_loaded('chronos-ext'));
    }

    /**
     * Process-level master switch: the extension is loaded AND configured on.
     * Framework integrations gate listener installation on this, so a fleet-wide
     * image with the .so baked in but CHRONOS_PHP_ENABLED unset costs an
     * application nothing — no listeners, no middleware body, no FFI per query.
     */
    public static function enabled(): bool
    {
        if (!self::loaded()) {
            return false;
        }
        if (self::$enabled === null) {
            // chronos_setting() resolves env > php.ini (chronos.*) > .chronos file;
            // older extensions predate it, where loaded implies "assume on" exactly
            // as before.
            $value = function_exists('chronos_setting')
                ? \chronos_setting('CHRONOS_PHP_ENABLED')
                : '1';
            self::$enabled = in_array(strtolower($value), ['1', 'true', 'yes', 'on'], true);
        }

        return self::$enabled;
    }

    /**
     * Whether a request is currently open in the native collector. Per-request
     * truth (sampling aside): false when disabled, mis-configured, or CLI without
     * CHRONOS_PHP_CLI_ENABLED. Bridges skip their per-request work when false.
     */
    public static function active(): bool
    {
        return self::loaded()
            && function_exists('chronos_request_active')
            && \chronos_request_active();
    }

    /**
     * Whether THIS request's HTTP stack is being captured. Bridges ask before
     * copying a response body across the FFI so an unsampled request never pays
     * for the copy. Older extensions lack the probe; capture was unconditional
     * there, so true preserves their behaviour.
     */
    public static function httpCapturing(): bool
    {
        if (!self::loaded()) {
            return false;
        }
        if (!function_exists('chronos_http_capturing')) {
            return true;
        }

        return \chronos_http_capturing();
    }

    /**
     * Whether captured message PAYLOADS are wanted at all.
     *
     * Deliberately not modelled on `httpCapturing()`, which is a per-request
     * sampling probe and answers TRUE against an older extension that lacks it
     * (capture was unconditional there, so true preserved the old behaviour).
     * This is a process-level operator decision, and its safe direction is the
     * other one: DEFAULT FALSE, so an older .so with no `chronos_setting()` —
     * and any deployment that simply has not set the flag — ships no payloads.
     * For a body, an unset setting must mean "do not send it", never "send it
     * because nobody said otherwise".
     *
     * `messaging_capture_bodies` defaults off where `http_capture_bodies`
     * defaults on because an inter-service payload is a data-sharing decision
     * the operator has not already made by installing an APM agent, and no
     * field-level masking applies to a body on either path — see
     * [`MessagingBody`].
     */
    public static function messagingCapturing(): bool
    {
        if (!self::loaded()) {
            return false;
        }
        if (self::$messagingCapture === null) {
            $value = function_exists('chronos_setting')
                ? \chronos_setting('CHRONOS_PHP_MESSAGING_CAPTURE_BODIES')
                : '';
            // The same truthiness list enabled() uses, so one spelling rule
            // covers every boolean setting an operator can write.
            self::$messagingCapture = in_array(strtolower($value), ['1', 'true', 'yes', 'on'], true);
        }

        return self::$messagingCapture;
    }

    /**
     * The most of one message body the operator has allowed, in bytes.
     *
     * The fallback is the Go SDK's own `CHRONOS_GO_MESSAGING_CAPTURE_MAX_BODY`
     * default rather than a number chosen here, so a Go producer and a PHP
     * producer on the same stream cap it identically — a stream where one side
     * truncates at 64 KiB and the other at some other figure is a stream whose
     * payloads cannot be compared.
     *
     * This is a CEILING on the operator's number, not the effective limit: each
     * call site min()s it against its own hard bound (a span attribute is capped
     * at 16 KiB, the request-attribute bag at 8 KiB), which is why
     * `MessagingBody::encode()` takes that bound as an argument.
     */
    public static function messagingBodyCeiling(): int
    {
        if (self::$messagingCeiling !== null) {
            return self::$messagingCeiling;
        }
        $configured = 0;
        if (function_exists('chronos_setting')) {
            $configured = (int) \chronos_setting('CHRONOS_PHP_MESSAGING_CAPTURE_MAX_BODY');
        }
        if ($configured <= 0) {
            $configured = 65536;
        }

        return self::$messagingCeiling = min($configured, 512 * 1024);
    }

    /**
     * Signal request start to the native extension with the inbound HTTP context.
     * The .so resolves config, parses traceparent, arms the profiler, activates DST.
     */
    public static function requestStart(
        ?string $traceparent,
        ?string $tracestate,
        ?string $baggage,
        ?string $sessionId,
        ?string $dstDirective,
        string $httpMethod,
        string $routePattern,
        string $serviceName,
    ): void {
        if (!self::loaded() || !function_exists('chronos_request_start')) {
            return;
        }
        \chronos_request_start(
            $traceparent ?? '',
            $tracestate ?? '',
            $baggage ?? '',
            $sessionId ?? '',
            $dstDirective ?? '',
            $httpMethod,
            $routePattern,
            $serviceName,
        );
        // When the collector declined the request (disabled, no envelope, CLI
        // opt-out), the rest of this bridge's per-request work is pure overhead.
        // Only a probe-capable extension can say so; an older .so keeps the
        // unconditional behaviour it always had.
        if (function_exists('chronos_request_active') && !\chronos_request_active()) {
            return;
        }
        SpanManager::reset();
        // The plan-capture budget is per REQUEST, but its counter is a static that
        // outlives one — same reason the span stack above needs resetting.
        QueryPlan::reset();
        self::armShutdownNet();
        self::loadInstrumentationManifest();
        if (class_exists(\Chronos\Collector\Framework\Laravel\RequestFacts::class)) {
            \Chronos\Collector\Framework\Laravel\RequestFacts::reset();
        }
    }

    /**
     * Load the application's instrumentation manifest (CHRONOS_PHP_INSTRUMENTATION_MANIFEST)
     * exactly once per process. The manifest calls Chronos\trace_method(), which registers
     * decorations in SpanDecorations AND allowlists each method in the native observer via
     * traceFunction() — both are per-process, so a single load covers every later request
     * on this worker.
     *
     * The env value may be relative (deepwell uses "chronos/instrumentation.php" against
     * an app root that is one level above the front controller's cwd), so several bases
     * are tried best-effort. Entirely fail-open: a missing file is a silent no-op.
     */
    private static function loadInstrumentationManifest(): void
    {
        if (self::$manifestLoaded) {
            return;
        }
        self::$manifestLoaded = true;
        try {
            $manifest = getenv('CHRONOS_PHP_INSTRUMENTATION_MANIFEST');
            if (($manifest === false || $manifest === '') && isset($_SERVER['CHRONOS_PHP_INSTRUMENTATION_MANIFEST'])) {
                $manifest = (string) $_SERVER['CHRONOS_PHP_INSTRUMENTATION_MANIFEST'];
            }
            if (($manifest === false || $manifest === '') && function_exists('chronos_setting')) {
                // The unified native settings layer also reads php.ini (chronos.*)
                // and the application's .chronos file.
                $manifest = \chronos_setting('CHRONOS_PHP_INSTRUMENTATION_MANIFEST');
            }
            if (!is_string($manifest) || $manifest === '') {
                return;
            }
            foreach (self::manifestCandidates($manifest) as $candidate) {
                if (is_file($candidate) && is_readable($candidate)) {
                    require_once $candidate;
                    return;
                }
            }
        } catch (\Throwable) {
            // A broken manifest must never take down the application.
        }
    }

    /** @return list<string> */
    private static function manifestCandidates(string $path): array
    {
        if ($path[0] === '/') {
            return [$path];
        }
        $candidates = [$path]; // relative to cwd as-is
        $cwd = getcwd();
        if (is_string($cwd) && $cwd !== '') {
            $candidates[] = $cwd.'/'.$path;
            $candidates[] = $cwd.'/../'.$path; // front controller runs from web/, app root is one up
        }
        if (isset($_SERVER['SCRIPT_FILENAME']) && $_SERVER['SCRIPT_FILENAME'] !== '') {
            $candidates[] = \dirname((string) $_SERVER['SCRIPT_FILENAME']).'/../'.$path;
        }
        if (isset($_SERVER['DOCUMENT_ROOT']) && $_SERVER['DOCUMENT_ROOT'] !== '') {
            $candidates[] = rtrim((string) $_SERVER['DOCUMENT_ROOT'], '/').'/../'.$path;
        }

        return array_values(array_unique($candidates));
    }

    /**
     * Register a function/method name into the native observer's span allowlist so the
     * Zend observer emits a span for its calls (chronos.instrumented=manifest). Names
     * are "Class::method" or a plain "function_name".
     */
    public static function traceFunction(string $name): void
    {
        if (!self::loaded() || !function_exists('chronos_trace_function')) {
            return;
        }
        try {
            \chronos_trace_function($name);
        } catch (\Throwable) {
        }
    }

    /**
     * Fatal-error safety net: a fatal (E_ERROR, compile error, OOM, timeout) unwinds
     * past every framework catch block, but shutdown functions still run. The native
     * chronos_request_end is idempotent, so on a healthy request this is a no-op —
     * the framework hook already flushed and cleared the request state.
     *
     * Registered on every requestStart under a per-request SAPI: PHP clears shutdown
     * functions between FPM requests, while class statics persist per worker — a
     * once-per-worker guard would leave every request after the first unprotected.
     *
     * Under CLI it is registered ONCE per process instead, and that distinction is
     * not a micro-optimisation. The real invariant is "at most one net per PHP
     * REQUEST", which the unguarded version expressed correctly only because a
     * request is the unit that clears the list. CLI breaks the equivalence: there is
     * exactly one PHP request for the whole process, so a worker that opens a
     * Chronos request per unit of work — a queue job, a consumed AMQP message — was
     * registering one closure per message that nothing could ever release. Measured
     * at roughly 470 bytes each: about 390 MiB/day for a consumer taking ten
     * messages a second, until memory_limit kills the worker mid-message.
     *
     * Arming once is not weaker cover, because the closure is STATELESS: it reads
     * error_get_last() and closes whichever request is open at the moment it fires,
     * so one registration protects every message the process ever handles.
     *
     * The test is on the SAPI rather than on "have we armed already", because those
     * are different questions and only the SAPI one is answerable here. `cli` and
     * `phpdbg` are the SAPIs with one request per process. `cli-server`
     * deliberately is not — it runs a real request cycle per connection and clears
     * the list like FPM. Octane/RoadRunner/Swoole workers report `cli` and do NOT
     * clear shutdown functions between their own pseudo-requests, so once-per-process
     * is the correct answer there too.
     */
    private static function armShutdownNet(): void
    {
        if (in_array(\PHP_SAPI, ['cli', 'phpdbg'], true)) {
            if (self::$shutdownNetArmed) {
                return;
            }
            self::$shutdownNetArmed = true;
        }
        register_shutdown_function(static function (): void {
            $error = error_get_last();
            $fatal = $error !== null && in_array(
                $error['type'],
                [E_ERROR, E_PARSE, E_CORE_ERROR, E_COMPILE_ERROR],
                true,
            );
            try {
                if ($fatal) {
                    // A fatal is unhandled by definition and exits 255; there is no
                    // throwable, so there is no throwable code to report.
                    \chronos_request_end(
                        500,
                        '',
                        'FatalError',
                        (string) ($error['message'] ?? ''),
                        ($error['file'] ?? '').':'.($error['line'] ?? ''),
                        '',
                        255,
                        false,
                    );
                } else {
                    \chronos_request_end(0, '');
                }
            } catch (\Throwable) {
                // Never let telemetry interfere with process shutdown.
            }
        });
    }

    /**
     * Merge extra attributes onto the native request root span. Framework bridges
     * use this for facts the .so cannot observe (Laravel route action, auth id,
     * view/model counts). No-op without the extension or when the request is inert.
     *
     * @param array<string, string> $attributes
     */
    public static function setRequestAttributes(array $attributes): void
    {
        if (!self::loaded() || !function_exists('chronos_set_request_attributes')) {
            return;
        }
        $safe = [];
        foreach ($attributes as $key => $value) {
            if (!is_string($key) || $key === '' || !is_scalar($value)) {
                continue;
            }
            $safe[$key] = (string) $value;
        }
        if ($safe === []) {
            return;
        }
        try {
            \chronos_set_request_attributes($safe);
        } catch (\Throwable) {
        }
    }

    /**
     * Declare the application's language / framework / release identity for this
     * request, stamped by the .so onto every span as the `app.*` attribute family.
     * Only userland can read PHP_VERSION and a framework's version constant
     * cheaply, so the bridge supplies them; empty strings leave config values
     * intact. Silently inert against an older .so that lacks the function.
     */
    public static function setAppMetadata(
        string $framework = '',
        string $frameworkVersion = '',
        string $appVersion = '',
    ): void {
        if (!self::loaded() || !function_exists('chronos_set_app_metadata')) {
            return;
        }
        try {
            \chronos_set_app_metadata(PHP_VERSION, $framework, $frameworkVersion, $appVersion);
        } catch (\Throwable) {
        }
    }

    /**
     * Mark the start of a named request phase for the trace Timeline tab.
     *
     * The mark names the phase that is BEGINNING. Everything before the first mark
     * is "bootstrap" and everything after the last runs to the end of the request,
     * so a bridge only declares boundaries it actually knows about — it never has
     * to close a phase it did not open.
     *
     * This is framework knowledge the .so cannot infer: only the framework knows
     * when routing ended and dispatch began.
     */
    public static function markPhase(string $name): void
    {
        if (!self::loaded() || !function_exists('chronos_mark_phase')) {
            return;
        }
        try {
            \chronos_mark_phase($name);
        } catch (\Throwable) {
        }
    }

    /**
     * Hand the collector the response body and headers about to be sent, so the
     * trace can show a rendered Response tab.
     *
     * The headers matter as much as the body: this runs BEFORE the framework
     * flushes, so the collector's own `headers_list()` read is still empty.
     *
     * The .so reads every other part of the HTTP stack itself, but by the time
     * request-end runs the body has gone to the SAPI and PHP kept no copy. A bridge
     * holding a Response object can supply it exactly and for free — no output
     * buffer, so streamed and X-Sendfile responses are untouched.
     *
     * Guarded by size before the call, from the collector's OWN configured total
     * (`CHRONOS_PHP_HTTP_CAPTURE_MAX_BODY_TOTAL`), so the .so stays the single
     * place the real limit is set — which is what this guard is for, and what a
     * media-type-specific number here had quietly stopped being. A fixed 1 MiB for
     * everything but HTML meant a 5 MB JSON export arrived already cut, and
     * arrived looking COMPLETE: the .so measures the size of what it is handed, so
     * the real length was gone before anything could report it.
     */
    /** @param array<string, string> $headers */
    public static function setResponseBody(
        ?string $body,
        string $contentType = '',
        array $headers = [],
    ): void {
        if (!self::loaded() || !function_exists('chronos_set_http_response_body')) {
            return;
        }
        $body = is_string($body) ? $body : '';
        if ($body === '' && $headers === []) {
            return;
        }
        try {
            \chronos_set_http_response_body(
                substr($body, 0, self::bodyCaptureCeiling()),
                $contentType,
                $headers,
            );
        } catch (\Throwable) {
        }
    }

    /**
     * The most the collector could keep of one body, plus a little slack.
     *
     * Slack because the .so applies the real cap itself: handing it one byte over
     * is what makes `.truncated` true, and shaving the string to exactly the cap
     * would present an oversized body as a complete one. An extension that predates
     * the setting reports nothing, and falls back to the total this SDK release
     * was written against.
     */
    private static function bodyCaptureCeiling(): int
    {
        if (self::$bodyCeiling !== null) {
            return self::$bodyCeiling;
        }
        $configured = 0;
        if (function_exists('chronos_setting')) {
            $configured = (int) \chronos_setting('CHRONOS_PHP_HTTP_CAPTURE_MAX_BODY_TOTAL');
        }
        if ($configured <= 0) {
            $configured = 8 * 1024 * 1024;
        }

        return self::$bodyCeiling = min($configured + 1, 512 * 1024 * 1024);
    }

    /**
     * Flatten a framework header bag to name => value.
     *
     * Symfony-style bags hold a LIST of values per name (Set-Cookie legitimately
     * repeats); they are joined rather than reduced to the first, because "which
     * cookies did this response set" is usually the whole question.
     *
     * @param iterable<string, string|list<string>|null> $bag
     * @return array<string, string>
     */
    public static function flattenHeaders(iterable $bag): array
    {
        $headers = [];
        foreach ($bag as $name => $value) {
            if (!is_string($name)) {
                continue;
            }
            if (is_array($value)) {
                $value = implode(', ', array_filter($value, 'is_scalar'));
            }
            if ($value === null || is_array($value)) {
                continue;
            }
            $headers[$name] = (string) $value;
        }

        return $headers;
    }

    /**
     * Signal request end. The .so flushes spans, profiles, logs, DST, metrics
     * to the spool directory. A caught throwable attaches error identity
     * (error.type/message/stack/code) to the request root span. `$handled`
     * records whether the framework rendered the throwable into a response
     * (true) or it escaped to the middleware boundary (false).
     */
    public static function requestEnd(
        int $httpStatusCode,
        string $routePattern = '',
        ?\Throwable $exception = null,
        ?bool $handled = null,
    ): void {
        if (!self::loaded() || !function_exists('chronos_request_end')) {
            return;
        }
        try {
            if ($exception !== null) {
                \chronos_request_end(
                    $httpStatusCode,
                    $routePattern,
                    get_class($exception),
                    $exception->getMessage(),
                    $exception->getFile().':'.$exception->getLine()."\n".$exception->getTraceAsString(),
                    // Cast, not int: PDOException and friends carry a non-numeric
                    // code (SQLSTATE `42S02`), which an int cast would flatten to 0.
                    (string) $exception->getCode(),
                    null,
                    $handled,
                );
            } else {
                \chronos_request_end($httpStatusCode, $routePattern);
            }
        } catch (\Throwable) {
            // An older .so without the error arguments still gets the plain flush.
            try {
                \chronos_request_end($httpStatusCode, $routePattern);
            } catch (\Throwable) {
            }
        }
    }

    /**
     * Append a finished userland span (SpanManager, Doctrine listeners) into the
     * native span batch so custom spans ship with the same trace.
     */
    public static function recordSpan(\Chronos\Collector\Dto\SpanRecord $record): void
    {
        if (!self::loaded() || !function_exists('chronos_record_span')) {
            return;
        }
        try {
            \chronos_record_span(
                $record->traceId,
                $record->spanId,
                $record->parentSpanId,
                $record->name,
                $record->startedAt,
                $record->endedAt,
                $record->attributes,
                $record->status,
            );
        } catch (\Throwable) {
            // Fail open: a span that cannot bridge must never break the request.
        }
    }

    /**
     * Forward a log record to the native extension for batched spool write.
     */
    public static function captureLog(
        string $severityText,
        int $severityNumber,
        string $body,
        array $attributes,
    ): void {
        if (!self::loaded() || !function_exists('chronos_capture_log')) {
            return;
        }
        \chronos_capture_log($severityText, $severityNumber, $body, $attributes);
    }

    /**
     * Announce that a queued job has STARTED, so it can be seen while it runs.
     *
     * Every other signal is spooled once the work is over, which is why a job in
     * flight is invisible — there is no span for it until it ends. This writes a
     * marker immediately; the job's own root span closes it out later, matched on
     * the trace and span ids the marker carries.
     *
     * `$timeoutSeconds` is the framework's own timeout, or 0 where it reports
     * none: a job cannot legitimately outlive it, so it is the only honest
     * deadline for presuming a worker that never came back is dead.
     *
     * @param array<string, string> $facts the root span's `messaging.*` attributes
     */
    public static function jobStarted(string $name, int $timeoutSeconds, array $facts): void
    {
        if (!self::loaded() || !function_exists('chronos_job_started')) {
            return;
        }
        try {
            \chronos_job_started($name, $timeoutSeconds, $facts);
        } catch (\Throwable) {
            // Fail open, like every other bridge here: a job must not fail
            // because the note saying it started could not be written.
        }
    }

    /**
     * Announce that userland instrumentation owns a data-access kind ("sql" |
     * "cache") for this request — the native observer stops emitting its
     * fallback I/O spans so the same query is never captured twice.
     */
    public static function suppressNative(string $kind): void
    {
        if (!self::loaded() || !function_exists('chronos_suppress_native')) {
            return;
        }
        try {
            \chronos_suppress_native($kind);
        } catch (\Throwable) {
        }
    }

    /**
     * Record a DST effect from PHP userland (DB query result, cache hit, etc.).
     */
    public static function recordDstEffect(string $kind, array $payload): void
    {
        if (!self::loaded() || !function_exists('chronos_record_dst')) {
            return;
        }
        \chronos_record_dst($kind, $payload);
    }

    /**
     * Get the outbound traceparent header for downstream propagation.
     */
    public static function traceparent(): ?string
    {
        if (!self::loaded() || !function_exists('chronos_traceparent')) {
            return null;
        }
        $value = \chronos_traceparent();
        return is_string($value) && $value !== '' ? $value : null;
    }

    /**
     * Get a child traceparent for manual outbound propagation (e.g. queue
     * messages, custom HTTP clients).
     */
    public static function childTraceparent(): ?string
    {
        if (!self::loaded() || !function_exists('chronos_child_traceparent')) {
            return null;
        }
        $value = \chronos_child_traceparent();
        return is_string($value) && $value !== '' ? $value : null;
    }

    /**
     * Retrieve the pending traceparent that the native observer prepared for
     * the current curl call. Returns null if none is pending.
     */
    public static function pendingCurlTraceparent(): ?string
    {
        if (!self::loaded() || !function_exists('chronos_pending_traceparent')) {
            return null;
        }
        $value = \chronos_pending_traceparent();
        return is_string($value) && $value !== '' ? $value : null;
    }

    /** Test seam. */
    public static function reset(): void
    {
        self::$loaded = null;
    }
}
