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
    }

    /**
     * Signal request end. The .so flushes spans, profiles, logs, DST, metrics
     * to the spool directory.
     */
    public static function requestEnd(int $httpStatusCode, string $routePattern = ''): void
    {
        if (!self::loaded() || !function_exists('chronos_request_end')) {
            return;
        }
        \chronos_request_end($httpStatusCode, $routePattern);
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
