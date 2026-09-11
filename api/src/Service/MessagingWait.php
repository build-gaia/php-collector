<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * How long a message waited between being sent and being picked up.
 *
 * The wait is the fact a queue is usually judged on, and NOTHING else can
 * supply it: the two halves of a message run in different processes, so the
 * consumer cannot know when the message was pushed unless the message says. A
 * backed-up queue and a slow handler produce the same handler duration and are
 * told apart only by this number.
 *
 * That forces a WALL clock, deliberately, despite being the worse clock: a
 * monotonic reading is meaningless in another process, so the only comparable
 * instant is the one both machines claim about the same world. Everything
 * awkward below follows from that choice.
 *
 * This logic began inside `Framework\Laravel\QueueTelemetry`, where the stamp
 * rides in the job payload. It lives in `Service/` now because the reasoning is
 * about clocks, not about Laravel: an AMQP bridge stamping the same instant into
 * a wire header needs the identical rule, and importing a Laravel class into an
 * AMQP bridge to get it would be the wrong dependency direction (a
 * `Framework/` class may reach into `Service/`, never the reverse).
 * `QueueTelemetry::waitMilliseconds()` keeps its public signature and delegates
 * here, so its existing callers and tests are unaffected.
 */
final class MessagingWait
{
    /**
     * How long the message waited, in milliseconds, or null when that cannot be
     * said honestly.
     *
     * Null rather than zero in every unknowable case — a message pushed before
     * this SDK was installed, a stamp another producer wrote in some other
     * format, or a clock difference that puts the dispatch AFTER the start. Zero
     * is a measurement meaning "picked up instantly", and a queue that is
     * actually unmeasured must not be able to report the healthiest possible
     * value. That is also why a negative reading is discarded whole instead of
     * clamped: skew of a second in one direction is skew of a second in the
     * other, so the positive readings from a skewed pair are wrong by as much as
     * the negative ones — the difference is only that clamping HIDES it.
     *
     * This is dispatch-to-start, so a deliberately delayed message counts its
     * delay as wait. The intent lives on the dispatching request instead, as the
     * `delay_ms` of its `messaging.jobs` catalog record: the two are in one trace
     * and can be read together, whereas a consumer holding only the message
     * cannot tell an intentional delay from a backlog.
     */
    public static function milliseconds(mixed $enqueuedAt, float $startedAt): ?int
    {
        if (!is_string($enqueuedAt) && !is_int($enqueuedAt) && !is_float($enqueuedAt)) {
            return null;
        }
        if (is_string($enqueuedAt) && !is_numeric($enqueuedAt)) {
            return null;
        }
        $enqueued = (float) $enqueuedAt;
        if (!\is_finite($enqueued) || $enqueued <= 0.0) {
            return null;
        }
        $waited = ($startedAt - $enqueued) * 1000.0;
        if (!\is_finite($waited) || $waited < 0.0) {
            return null;
        }

        return (int) \round($waited);
    }
}
