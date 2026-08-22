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

    /** Once-per-process guard for the instrumentation-manifest load (the native
     * trace allowlist is per-process and only ever grows, so once is enough). */
    private static bool $manifestLoaded = false;

    public static function loaded(): bool
    {
        return self::$loaded ??= extension_loaded('chronos-ext');
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
        SpanManager::reset();
        // The plan-capture budget is per REQUEST, but its counter is a static that
        // outlives one — same reason the span stack above needs resetting.
        QueryPlan::reset();
        self::armShutdownNet();
        self::loadInstrumentationManifest();
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
     * Registered on every requestStart: PHP clears shutdown functions between FPM
     * requests, while class statics persist per worker — a once-per-worker guard
     * would leave every request after the first unprotected.
     */
    private static function armShutdownNet(): void
    {
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
     * Hand the collector the response body about to be sent, so the trace can show
     * a rendered Response tab.
     *
     * The .so reads every other part of the HTTP stack itself, but by the time
     * request-end runs the body has gone to the SAPI and PHP kept no copy. A bridge
     * holding a Response object can supply it exactly and for free — no output
     * buffer, so streamed and X-Sendfile responses are untouched.
     *
     * Guarded by size before the call: shipping a 40 MB CSV export across the FFI
     * boundary only for the .so to truncate it to 64 KiB is pure waste. The cap
     * here is deliberately generous relative to the collector's own so the .so
     * stays the single place the real limit is configured.
     */
    public static function setResponseBody(?string $body, string $contentType = ''): void
    {
        if (!self::loaded() || !function_exists('chronos_set_http_response_body')) {
            return;
        }
        if (!is_string($body) || $body === '') {
            return;
        }
        try {
            \chronos_set_http_response_body(substr($body, 0, 1048576), $contentType);
        } catch (\Throwable) {
        }
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
