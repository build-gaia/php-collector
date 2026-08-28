<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\SpanManager;
use Throwable;

/**
 * Every exception the application reports, placed in the trace where it happened.
 *
 * The request root already carries the ONE exception that reached the response —
 * the middleware reads it off `$response->exception` and hands it to
 * `requestEnd()`. That is the exception the user saw, and for a long time it was
 * the only one a trace could show. It is routinely not the interesting one: an
 * exception caught, reported and recovered from is invisible by construction,
 * and a request that quietly swallowed six timeouts and returned 200 looked
 * identical to one that did no work at all.
 *
 * Laravel's `reportable()` hook fires for everything that reaches the handler,
 * recovered or not, so that is where these are read from. Each becomes its own
 * zero-duration span rather than an attribute on the root, because WHEN it was
 * thrown — after which query, inside which job — is most of what makes it
 * legible, and only a span carries that.
 */
final class ExceptionCapture
{
    /**
     * A request that throws in a loop should not be able to turn one trace into
     * thousands of spans. The cap is high enough that a real request never
     * reaches it and low enough that a runaway one stays readable; the overflow
     * is counted, never silently dropped.
     */
    private const MAX_SPANS = 16;

    private static int $recorded = 0;

    private static int $dropped = 0;

    public static function reset(): void
    {
        self::$recorded = 0;
        self::$dropped = 0;
    }

    /** How many reported exceptions exceeded the per-request span cap. */
    public static function droppedCount(): int
    {
        return self::$dropped;
    }

    /**
     * Record one reported exception.
     *
     * `handled` is what separates this from the root's own error attributes: an
     * exception that arrives here and never reaches the response was recovered
     * from, and a reader who cannot tell those apart will chase the wrong one.
     */
    public static function record(Throwable $exception, bool $handled = true): void
    {
        try {
            // Counted BEFORE the cap is consulted. The cap bounds spans, which are
            // expensive; the count is one integer, and it is the number that tells
            // a reader the difference between a request that failed once and one
            // that failed in a loop. Capping it too would hide exactly the case
            // the cap exists to survive.
            RequestFacts::noteException($exception::class);
            if (self::$recorded >= self::MAX_SPANS) {
                ++self::$dropped;

                return;
            }
            ++self::$recorded;
            $span = SpanManager::open('EXCEPTION '.self::shortName($exception::class));
            if (!$span->isVoid()) {
                $span->add('span.kind', 'internal');
                $span->add('error.handled', $handled ? 'true' : 'false');
                $span->add('code.filepath', $exception->getFile());
                $span->add('code.lineno', (string) $exception->getLine());
                // Rich: the stack is the reason this span is worth having. The
                // message is bounded by add() like every other text attribute.
                $span->recordException($exception, true);
                $span->markError();
            }
            $span->finish();
        } catch (Throwable) {
            // Instrumenting a failure must never become a second failure.
        }
    }

    /** `QueryException`, not `Illuminate\Database\QueryException`, for the span name. */
    private static function shortName(string $class): string
    {
        $position = strrpos($class, '\\');
        $short = $position === false ? $class : substr($class, $position + 1);

        // Span::open caps the name itself, so no second bound is needed here.
        return $short === '' ? 'Exception' : $short;
    }
}
