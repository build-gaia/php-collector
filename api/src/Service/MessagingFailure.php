<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Throwable;

/**
 * The throwable a consumer caught and chose to carry on from, held until the
 * delivery's span is closed.
 *
 * ## The failure this fixes
 *
 * A consumer that catches every `Throwable`, reports it to its own exception
 * handler and then ACKS the message closes its span as a SUCCESS: no
 * `error.type`, no `error.message`, `isError` false. The work failed and the
 * telemetry says it went fine — which makes a per-queue error rate meaningless
 * and hides the one class of failure a queue is most often judged on.
 *
 * ## Why a slot rather than a rethrow
 *
 * The requirement is that instrumentation changes neither propagation nor
 * acking. Rethrowing so the bridge's existing catch sees it would change both:
 * the throwable would escape into the broker client's event loop, and the ack
 * that follows the handler would never run. Marking from INSIDE the
 * application's own catch changes only the span's status.
 *
 * ## Why not derive it from the exception handler
 *
 * Laravel's `reportable()` hook already produces an EXCEPTION child span with
 * `error.handled=true`, and it deliberately does not touch the root's status.
 * An application may report an exception and still succeed — a fallback, a
 * retry, a partial write — so promoting every reported exception to a failed
 * delivery would over-report and make the same error rate useless from the other
 * direction. The explicit call in the catch is the only place that knows the
 * message was abandoned.
 *
 * ## Mechanism
 *
 * `chronos_request_end` derives `errored` from exactly one input — a non-empty
 * error type argument (`native/src/lib.rs`: `let errored = !error_type.is_empty()`).
 * Attributes cannot flip a span's status, and there is no native API to mark an
 * already-open request. So the throwable is parked here and read by the bridge
 * immediately before its final `requestEnd`, which then passes the full error
 * identity the native signature already accepts. No new FFI function, no .so
 * rebuild.
 *
 * Deliberately NOT a static on BunnyTelemetry: Laravel's QueueTelemetry and
 * Messenger's middleware have the identical problem, and a slot living inside a
 * `class_exists(\Bunny\Channel::class)` guard would be unreachable for them.
 */
final class MessagingFailure
{
    private static ?Throwable $failure = null;

    /**
     * Note that the delivery currently being processed FAILED, even though the
     * application is going to carry on and ack it.
     *
     * Last writer wins: a handler that catches, retries and fails again should
     * report the failure it actually gave up on.
     */
    public static function note(Throwable $failure): void
    {
        self::$failure = $failure;
    }

    /** Take the noted failure, clearing it. Null when the delivery succeeded. */
    public static function take(): ?Throwable
    {
        $failure = self::$failure;
        self::$failure = null;

        return $failure;
    }

    /**
     * Clear the slot at the START of a delivery.
     *
     * Not optional housekeeping: this is a static inside a long-lived worker
     * process, so a failure nobody took would be attributed to the NEXT message
     * — a green message reported as red, which is worse than the bug this class
     * fixes.
     */
    public static function reset(): void
    {
        self::$failure = null;
    }
}
