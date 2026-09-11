<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * The collector complaining about ITSELF, once per process.
 *
 * Every path in this SDK is fail-open, and that is not negotiable:
 * instrumentation must never change what an application publishes, consumes or
 * throws. But fail-open was quietly read as fail-SILENT, and the two are not the
 * same thing. A spool directory the worker could not write discarded every
 * request's telemetry for a whole day with no complaint anywhere — a collector
 * losing everything is indistinguishable, from the outside, from a collector
 * that was never switched on, and that cost hours of debugging in the wrong
 * place.
 *
 * ## Once per process, never per call
 *
 * The native side already has the right idiom (`spool_log`'s
 * `[chronos-ext] spool budget reached: …`), and the rule that comes with it
 * matters as much as the message: a warning on a hot path is its own outage. A
 * per-publish or per-delivery line would fill a container log's disk while
 * describing one steady condition. So a `$key` is latched for the life of the
 * process and every later occurrence is silent.
 *
 * Under PHP-FPM "once per process" means once per WORKER: a 32-worker pool
 * prints up to 32 identical lines when the condition first appears. That is the
 * correct trade — each worker has its own state and its own losses — but the
 * wording says so, because the repetition would otherwise read as a loop.
 *
 * ## error_log, and specifically NOT trigger_error
 *
 * Laravel (and Symfony's debug error handler) installs a `set_error_handler`
 * that converts `E_USER_WARNING` into an `ErrorException`. A `trigger_error` on
 * a fail-open path would therefore turn a telemetry failure into an application
 * exception — the exact outage the fail-open design exists to prevent.
 * `error_log()` writes to the SAPI log, which is where the extension's own
 * `[chronos-ext]` lines already land (`docker logs`), so the two halves of the
 * collector complain in one place.
 *
 * ## Only what an operator would act on
 *
 * A warning is worth its line only if someone would do something about it. The
 * bridges deliberately keep most of their catches silent: a supported
 * configuration (no .so installed, no SDK in a `--no-dev` deploy), or a loss
 * that leaves the span itself intact and only its notes thinner. What gets a
 * line is a failure whose consequence an operator would otherwise spend hours
 * chasing — no telemetry at all, one giant trace, a missing publish half, an
 * allowlist silently not in effect.
 */
final class Diagnostics
{
    /** @var array<string, true> */
    private static array $announced = [];

    /**
     * Announce a collector-internal failure, at most once per `$key` per process.
     *
     * Fail-open itself, and that is not belt-and-braces: this is called from
     * inside catch blocks on the request path, and a diagnostic that threw would
     * turn the failure it was describing into an application error.
     */
    public static function warnOnce(string $key, string $message): void
    {
        try {
            if (isset(self::$announced[$key])) {
                return;
            }
            self::$announced[$key] = true;
            \error_log('[chronos] '.$message.' (reported once per process; every PHP worker is its own process)');
        } catch (\Throwable) {
            // Nothing left to say, and saying it must not be what breaks.
        }
    }

    /** Test seam: forget what has been announced, so a case can observe the first line. */
    public static function reset(): void
    {
        self::$announced = [];
    }

    /** Whether `$key` has already been announced in this process. Test seam. */
    public static function announced(string $key): bool
    {
        return isset(self::$announced[$key]);
    }
}
