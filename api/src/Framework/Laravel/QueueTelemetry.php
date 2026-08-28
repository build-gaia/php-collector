<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\NativeExtension;
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
 * Requires `CHRONOS_PHP_CLI_ENABLED`: the .so's RINIT hook deliberately skips CLI
 * processes, so without it a worker's requestStart is declined and this all stays
 * inert.
 */
final class QueueTelemetry
{
    /** The payload key the trace context travels under. */
    public const PAYLOAD_KEY = 'chronos';

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
     * The trace context stamped into every outgoing job payload.
     *
     * A CHILD traceparent, not this request's own: the job is caused by the
     * dispatching request but is not part of it, so it hangs beneath the
     * dispatch rather than claiming to be the same span — the same shape an
     * outbound HTTP call gets.
     *
     * @return array<string, array<string, string>>
     */
    public static function payloadContext(): array
    {
        try {
            $traceparent = NativeExtension::childTraceparent() ?? NativeExtension::traceparent();
            if ($traceparent === null || $traceparent === '') {
                return [];
            }

            return [self::PAYLOAD_KEY => ['traceparent' => $traceparent]];
        } catch (Throwable) {
            return [];
        }
    }

    /** Begin a job-scoped request, continuing the dispatcher's trace when it left one. */
    private static function openJob(object $event): void
    {
        try {
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
                (string) config('app.name', 'laravel'),
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
            NativeExtension::setRequestAttributes(self::jobAttributes($event, $job, $name));
        } catch (Throwable) {
        }
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
     * @return array<string, string>
     */
    private static function jobAttributes(object $event, object $job, string $name): array
    {
        $attributes = [
            'span.kind' => 'consumer',
            'messaging.operation' => 'process',
            'messaging.message.name' => $name,
        ];
        try {
            $connection = is_string($event->connectionName ?? null) ? $event->connectionName : '';
            $transport = self::queueDriver($connection);
            if ($transport !== '') {
                $attributes['messaging.system'] = $transport;
            }
            if (method_exists($job, 'getQueue')) {
                $queue = (string) $job->getQueue();
                if ($queue !== '') {
                    $attributes['messaging.destination.name'] = $queue;
                }
            }
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
