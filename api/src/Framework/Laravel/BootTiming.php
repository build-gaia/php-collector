<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\SpanManager;
use DateTimeImmutable;
use DateTimeZone;
use Throwable;

/**
 * Framework bootstrap, measured and turned into one span.
 *
 * The Timeline tab's phase marks cannot describe this stretch. A phase mark is
 * only accepted once the native request context exists, and every provider has
 * already booted by the time the collector's middleware runs — so the whole of
 * bootstrap is, from the phase timeline's point of view, the unnamed prefix
 * before the first mark. That prefix is often the largest single block of a slow
 * request, and "everything before dispatch" is not an answer to why.
 *
 * So bootstrap is reported as a span instead, with both ends measured rather than
 * assumed: it starts at `LARAVEL_START` (the first line of the front controller)
 * and ends when the container fires its `booted` callbacks. The span is emitted
 * later, from the middleware, because only then does a trace context exist to
 * hang it on — `backdateStart()` and `finishAt()` place it back where it actually
 * happened rather than where it was recorded.
 */
final class BootTiming
{
    /** Wall clock at which the container finished booting every provider. */
    private static ?float $bootedAt = null;

    /** How many service providers the container had loaded by then. */
    private static int $providerCount = 0;

    /** Emitted at most once per request, however many bridges ask. */
    private static bool $emitted = false;

    /**
     * Called from the container's `booted` callback — after every provider's
     * boot() has run, which is the only moment that boundary is observable.
     */
    public static function markBooted(int $providerCount = 0): void
    {
        self::$bootedAt = microtime(true);
        self::$providerCount = $providerCount;
    }

    /** Clear per-request state; the statics outlive one request inside a worker. */
    public static function reset(): void
    {
        self::$bootedAt = null;
        self::$providerCount = 0;
        self::$emitted = false;
    }

    /**
     * Emit the bootstrap span, if bootstrap was observed.
     *
     * Silent when `LARAVEL_START` is absent (a console kernel, an unusual front
     * controller) rather than guessing a start: a bootstrap span whose start time
     * was invented would misattribute time that belongs somewhere else, and a
     * missing span is easier to read than a wrong one.
     */
    public static function emitBootSpan(): void
    {
        if (self::$emitted || self::$bootedAt === null) {
            return;
        }
        self::$emitted = true;
        try {
            $startedAt = self::requestStartedAt();
            if ($startedAt === null || self::$bootedAt <= $startedAt) {
                return;
            }
            $elapsedMs = (self::$bootedAt - $startedAt) * 1000;
            $span = SpanManager::open('BOOT laravel');
            $span->backdateStart($elapsedMs);
            if (!$span->isVoid()) {
                $span->add('span.kind', 'internal');
                $span->add('app.framework', 'laravel');
                $span->add('framework.boot.duration_ms', sprintf('%.3F', $elapsedMs));
                if (self::$providerCount > 0) {
                    $span->add('framework.boot.providers', (string) self::$providerCount);
                }
            }
            $span->finishAt(self::formatTimestamp(self::$bootedAt));
        } catch (Throwable) {
        }
    }

    /**
     * When the PHP process began serving this request.
     *
     * `LARAVEL_START` is defined on the first line of Laravel's own front
     * controller, so it is the earliest instant userland can see, and it precedes
     * autoloading. `REQUEST_TIME_FLOAT` is the fallback: the SAPI's own stamp,
     * marginally earlier and just as real.
     */
    private static function requestStartedAt(): ?float
    {
        if (defined('LARAVEL_START')) {
            $start = constant('LARAVEL_START');
            if (is_float($start) || is_int($start)) {
                return (float) $start;
            }
        }
        $sapi = $_SERVER['REQUEST_TIME_FLOAT'] ?? null;

        return is_float($sapi) || is_int($sapi) ? (float) $sapi : null;
    }

    /** A microtime float in the wire format Span uses for its own timestamps. */
    private static function formatTimestamp(float $microtime): ?string
    {
        $stamp = DateTimeImmutable::createFromFormat('U.u', sprintf('%.6F', $microtime));
        if ($stamp === false) {
            return null;
        }

        return $stamp->setTimezone(new DateTimeZone('UTC'))->format('Y-m-d\TH:i:s.u\Z');
    }
}
