<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\MessagingDestination;
use Chronos\Collector\Service\MessagingWait;
use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\SpanManager;
use Illuminate\Queue\Events\JobExceptionOccurred;
use Illuminate\Queue\Events\JobFailed;
use Illuminate\Queue\Events\JobProcessed;
use Illuminate\Queue\Events\JobProcessing;
use Illuminate\Queue\Queue;
use Illuminate\Support\Facades\Event;
use Throwable;

/**
 * The two halves of a queued job, joined into one trace.
 *
 * A dispatch used to be the end of the story: the request root recorded that it
 * had queued `IndexUser` and nothing after that could be followed, because the
 * job runs minutes later in a different process with a trace context of its own —
 * if it had one at all. The work a request causes is still work the request
 * caused, and a trace that stops at the dispatch hides all of it.
 *
 * So the W3C context travels WITH the message. `createPayloadUsing` is Laravel's
 * own supported seam for exactly this, which matters more than convenience: the
 * context rides inside the payload the broker already carries, so nothing depends
 * on a particular queue driver keeping headers, and a job that is retried,
 * released or moved between queues keeps it.
 *
 * On the worker side each job opens and closes a request of its own. That is not
 * a metaphor — a job IS the unit a worker serves, with a beginning, an outcome
 * and its own queries and HTTP calls — so it reuses the request machinery rather
 * than inventing a parallel one, and everything already built on the request root
 * (facts, transactions, the DST recording) works inside a job unchanged.
 *
 * `CHRONOS_PHP_CLI_ENABLED` is NOT required, despite what this comment used to
 * say. That flag gates only the extension's AUTOMATIC request-start in RINIT;
 * `chronos_request_start` never consults it, so the explicit per-job start below
 * works on a worker with the flag unset (verified against a built extension: a
 * job emits its `.trace` with `QUEUE`, the consumer attributes and the wait, plus
 * its in-flight marker).
 *
 * Leaving the flag OFF is the better configuration, not merely an acceptable one.
 * With it on, RINIT opens a request when the worker PROCESS starts, and
 * `chronos_request_start` enriches an already-open request rather than opening a
 * new one — so the first job of every worker would inherit a trace containing
 * everything since boot, and an idle worker would accumulate spans toward the
 * 32,768-span ceiling with no job to attribute them to.
 *
 * What IS required is this class being installed — it is userland code, so an
 * application whose vendor tree lacks the SDK emits no job telemetry however the
 * extension is configured.
 */
final class QueueTelemetry
{
    /** The payload key the trace context travels under. */
    public const PAYLOAD_KEY = 'chronos';

    /** The key, inside `PAYLOAD_KEY`, holding the dispatch instant as epoch seconds. */
    public const ENQUEUED_AT_KEY = 'enqueued_at';

    private static bool $installed = false;

    /** True while a job-scoped request is open, so the close is never doubled. */
    private static bool $jobOpen = false;

    public static function install(): void
    {
        if (self::$installed || !NativeExtension::loaded()) {
            return;
        }
        self::$installed = true;

        try {
            if (class_exists(Queue::class) && method_exists(Queue::class, 'createPayloadUsing')) {
                Queue::createPayloadUsing(static fn (): array => self::payloadContext());
            }
            if (!class_exists(Event::class)) {
                return;
            }
            if (class_exists(JobProcessing::class)) {
                Event::listen(JobProcessing::class, static function (object $event): void {
                    self::openJob($event);
                });
            }
            if (class_exists(JobProcessed::class)) {
                Event::listen(JobProcessed::class, static function (object $event): void {
                    self::closeJob($event, null);
                });
            }
            // Both failure events are observed because they are not the same
            // event: JobExceptionOccurred fires on every attempt that throws,
            // JobFailed only once the job has exhausted its retries. A worker
            // that only watched the second would show a job that failed six times
            // as a job that failed once.
            if (class_exists(JobExceptionOccurred::class)) {
                Event::listen(JobExceptionOccurred::class, static function (object $event): void {
                    self::closeJob($event, $event->exception ?? null);
                });
            }
            if (class_exists(JobFailed::class)) {
                Event::listen(JobFailed::class, static function (object $event): void {
                    self::closeJob($event, $event->exception ?? null);
                });
            }
        } catch (Throwable) {
        }
    }

    /**
     * The trace context and dispatch instant stamped into every outgoing payload.
     *
     * The RESERVED PRODUCER SPAN's traceparent, so the job hangs beneath the
     * dispatch itself — the same shape an outbound HTTP call gets, and now
     * genuinely so. This used to call `NativeExtension::childTraceparent()`,
     * which mints an id that no span is ever recorded under, and that is why
     * every queued job on this estate was an ORPHAN: the worker's root span
     * carried a parent id naming nothing, so the job shared a trace with the
     * request that dispatched it and had no edge to it.
     *
     * The reservation is spent in a DIFFERENT call: this runs during `push`
     * (via `Queue::createPayloadUsing`), while the producer span is recorded from
     * the `JobQueued` event in `RequestFacts`. It recovers the id from the
     * payload rather than from a side channel here — see
     * `SpanReservation::fromTraceparent` on why the wire is the only alignment
     * that cannot hand a producer span another message's id.
     *
     * The `?? traceparent()` fallback is KEPT, and it is the tier this code
     * already got right by accident: the REQUEST ROOT is a span that really is
     * recorded, so a dispatch with no reservation available still nests its job
     * under the dispatching request. Less precise, still true. With neither, no
     * traceparent is stamped at all and the worker roots its own trace — a
     * traceparent on the wire is a promise, and a publisher with no open request
     * cannot make it.
     *
     * `enqueued_at` rides alongside it because the wait is the fact a queue is
     * usually judged on and NOTHING else can supply it: the two halves of a job
     * run in different processes, so the worker cannot know when the message was
     * pushed unless the message says. A backed-up queue and a slow job produce
     * the same job duration and are told apart only by this number.
     *
     * A WALL clock, deliberately, despite being the worse clock: a monotonic
     * reading is meaningless in another process, so the only comparable instant
     * is the one both machines claim about the same world. That makes the
     * difference vulnerable to clock skew between dispatcher and worker, which is
     * why the consumer side treats a negative wait as unknown rather than
     * clamping it to zero — see [`waitMilliseconds`].
     *
     * Present even when there is no traceparent: a job dispatched from a CLI
     * command with no open request has no trace to continue but has waited just
     * as long, and dropping the whole key would leave that wait unmeasurable.
     *
     * @return array<string, array<string, string>>
     */
    public static function payloadContext(): array
    {
        try {
            $context = [self::ENQUEUED_AT_KEY => \sprintf('%.6F', \microtime(true))];
            $traceparent = SpanManager::reserve()?->header() ?? NativeExtension::traceparent();
            if (is_string($traceparent) && $traceparent !== '') {
                $context['traceparent'] = $traceparent;
            }

            return [self::PAYLOAD_KEY => $context];
        } catch (Throwable) {
            return [];
        }
    }

    /**
     * How long the message waited, in milliseconds, or null when that cannot be
     * said honestly.
     *
     * The rule — and the clock-skew reasoning behind returning null rather than
     * zero, and discarding a negative reading rather than clamping it — now
     * lives in [`MessagingWait`], because it is reasoning about wall clocks
     * across processes rather than anything Laravel-specific: the AMQP bridge
     * stamps the same instant into a wire header and needs the identical rule,
     * and it cannot reach a `Framework\Laravel` class to get it.
     *
     * Kept here, with its signature unchanged, because it is public and already
     * covered by the suite: the delegation is what moves the logic without
     * churning callers or tests.
     */
    public static function waitMilliseconds(mixed $enqueuedAt, float $startedAt): ?int
    {
        return MessagingWait::milliseconds($enqueuedAt, $startedAt);
    }

    /** Begin a job-scoped request, continuing the dispatcher's trace when it left one. */
    private static function openJob(object $event): void
    {
        try {
            // Read before any of the work below, so the wait is not inflated by
            // the cost of measuring it.
            $startedAt = \microtime(true);
            $job = $event->job ?? null;
            if (!is_object($job)) {
                return;
            }
            $name = self::jobName($job);
            $payload = self::jobPayload($job);
            $traceparent = $payload[self::PAYLOAD_KEY]['traceparent'] ?? null;

            NativeExtension::requestStart(
                is_string($traceparent) ? $traceparent : null,
                null,
                null,
                null,
                null,
                // The queue is this job's "method and route": what it was, and
                // where it came from. Naming them in the HTTP fields keeps one
                // vocabulary for the root span rather than a second one only
                // workers use.
                'QUEUE',
                $name,
                // Empty service name → native falls back to
                // CHRONOS_PHP_APPLICATION, so a job and the web requests of the
                // same service land on ONE service map node.
                '',
            );
            if (!NativeExtension::active()) {
                return;
            }
            self::$jobOpen = true;
            NativeExtension::setAppMetadata(
                'laravel',
                \class_exists(\Illuminate\Foundation\Application::class) ? app()->version() : '',
                (string) config('app.version', ''),
            );
            // Same trade as the HTTP path: userland owns SQL and cache spans for
            // the duration, so the native fallbacks would only double them.
            NativeExtension::suppressNative('sql');
            NativeExtension::suppressNative('cache');
            ChronosViewEngine::resetRequestState();
            ExceptionCapture::reset();
            $attributes = self::jobAttributes($event, $job, $name, $payload, $startedAt);
            NativeExtension::setRequestAttributes($attributes);
            // Announced AFTER requestStart, never before: the marker's whole job
            // is to name the span that will later close it, and that span does
            // not exist until the request is open.
            NativeExtension::jobStarted($name, self::timeoutSeconds($job), $attributes);
        } catch (Throwable) {
        }
    }

    /**
     * The job's own timeout in seconds, or 0 when the framework reports none.
     *
     * Zero rather than a default, and the difference matters: this becomes the
     * deadline after which a job whose worker died is presumed dead, so a guessed
     * timeout reaps jobs that are still working. A queue whose jobs declare no
     * timeout gets in-flight rows that only a completing span closes, which is
     * the honest consequence of the application not saying.
     *
     * `retryUntil` wins where both exist: a job with a retry horizon may
     * legitimately be re-attempted past a single attempt's timeout, so the
     * horizon is the later — and therefore the safer — of the two.
     */
    private static function timeoutSeconds(object $job): int
    {
        try {
            if (method_exists($job, 'retryUntil')) {
                $until = $job->retryUntil();
                if (is_int($until) && $until > 0) {
                    $remaining = $until - time();
                    if ($remaining > 0) {
                        return $remaining;
                    }
                }
            }
        } catch (Throwable) {
        }
        try {
            if (method_exists($job, 'timeout')) {
                $timeout = $job->timeout();
                if (is_int($timeout) && $timeout > 0) {
                    return $timeout;
                }
            }
        } catch (Throwable) {
        }

        return 0;
    }

    /** End the job-scoped request, recording the throwable when there was one. */
    private static function closeJob(object $event, ?Throwable $exception): void
    {
        if (!self::$jobOpen) {
            return;
        }
        self::$jobOpen = false;
        try {
            RichTelemetryHooks::closeDanglingTransactions();
            RequestFacts::flush([
                'process.runtime.memory.peak_bytes' => (string) memory_get_peak_usage(true),
            ]);
            $job = $event->job ?? null;
            $route = is_object($job) ? self::jobName($job) : '';
            // Zero, not 200: a job has no HTTP status, and borrowing one would put
            // a number in the column that means something it does not mean. The
            // outcome is carried by the error attributes and messaging.* instead.
            NativeExtension::requestEnd(0, $route, $exception, $exception === null ? null : true);
        } catch (Throwable) {
        }
    }

    /**
     * Root-span attributes describing the message being processed, in the same
     * OTel `messaging.*` vocabulary MessagingSpan uses for the publish side — so
     * the producer and consumer halves of one queue join on the same keys.
     *
     * @param array<string, mixed> $payload the message as the broker carried it
     *
     * @return array<string, string>
     */
    private static function jobAttributes(
        object $event,
        object $job,
        string $name,
        array $payload,
        float $startedAt,
    ): array {
        $attributes = [
            'span.kind' => 'consumer',
            'messaging.operation' => 'process',
            'messaging.message.name' => $name,
        ];
        try {
            $waited = self::waitMilliseconds(
                $payload[self::PAYLOAD_KEY][self::ENQUEUED_AT_KEY] ?? null,
                $startedAt,
            );
            if ($waited !== null) {
                $attributes['messaging.message.queue_time_ms'] = (string) $waited;
            }
            $connection = is_string($event->connectionName ?? null) ? $event->connectionName : '';
            $transport = self::queueDriver($connection);
            if ($transport !== '') {
                $attributes['messaging.system'] = $transport;
            }
            $queue = method_exists($job, 'getQueue') ? (string) $job->getQueue() : '';
            // The same normalised destination the publish side writes, so the two
            // halves of one queue describe the same place in the same words —
            // and so a consume span can be joined to ONE stream on an estate
            // where the queue's name repeats across vhosts.
            $attributes += MessagingDestination::forLaravelQueue($transport, $connection, $queue);
            if (method_exists($job, 'getJobId')) {
                $id = $job->getJobId();
                if (is_scalar($id) && (string) $id !== '') {
                    $attributes['messaging.message.id'] = (string) $id;
                }
            }
            // The attempt number is the field that separates "slow" from "failing
            // and being retried", which look identical without it.
            if (method_exists($job, 'attempts')) {
                $attempts = $job->attempts();
                if (is_int($attempts) && $attempts > 0) {
                    $attributes['messaging.message.attempt'] = (string) $attempts;
                }
            }
        } catch (Throwable) {
        }

        return $attributes;
    }

    /** The application's name for the job, not the framework's wrapper class. */
    private static function jobName(object $job): string
    {
        try {
            if (method_exists($job, 'resolveName')) {
                $name = (string) $job->resolveName();
                if ($name !== '') {
                    return $name;
                }
            }
        } catch (Throwable) {
        }

        return $job::class;
    }

    /** @return array<string, mixed> */
    private static function jobPayload(object $job): array
    {
        try {
            if (method_exists($job, 'payload')) {
                $payload = $job->payload();

                return is_array($payload) ? $payload : [];
            }
        } catch (Throwable) {
        }

        return [];
    }

    /** The connection's driver, which is where to look; the name is not. */
    private static function queueDriver(string $connection): string
    {
        try {
            if ($connection === '' || !function_exists('config')) {
                return $connection;
            }
            $driver = config("queue.connections.{$connection}.driver");

            return is_string($driver) && $driver !== '' ? $driver : $connection;
        } catch (Throwable) {
            return $connection;
        }
    }
}
