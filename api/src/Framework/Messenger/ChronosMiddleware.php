<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Messenger;

use Chronos\Collector\Service\MessagingSpan;
use Chronos\Collector\Service\NativeExtension;
use Throwable;

/**
 * The two halves of a Symfony Messenger message, joined into one trace — the Messenger
 * counterpart of Laravel's QueueTelemetry, for the same underlying reason: a message that
 * leaves the process, whether over AMQP, Doctrine, Redis or the sync transport, is work the
 * dispatching request caused, and a trace that stops at dispatch hides all of it.
 *
 * One middleware, two lifecycles, told apart by `ReceivedStamp`'s presence — Messenger's own
 * signal that an envelope arrived from a worker's `receive()` rather than from `dispatch()`:
 *
 *   DISPATCH (no ReceivedStamp): a producer span, the same `MessagingSpan` used for Laravel
 *   broadcasting and any other publish, so the two halves of a queue join the Data Sources
 *   producer graph on the same `messaging.*` keys regardless of framework. Recorded AFTER
 *   `$stack->next()->handle()` returns, not before — same reason MessagingSpan is zero-duration
 *   by construction: this fires from the fact that the send already happened, and only the
 *   send itself (via `SendMessageMiddleware`, downstream of this one in the default bus) knows
 *   which transport the message actually went to.
 *
 *   CONSUME (ReceivedStamp present): a job-scoped request, continuing the traceparent this
 *   middleware stamped onto the envelope on the way out. Same trade as QueueTelemetry: the
 *   worker's HANDLE call is treated as the request root ('QUEUE' / the message class stand in
 *   for method / route) so everything already built on the request root — facts, DST recording,
 *   the SQL/cache suppression the HTTP path relies on — works inside a consumed message
 *   unchanged, rather than inventing a parallel lifecycle for workers.
 *
 * Requires `CHRONOS_PHP_CLI_ENABLED`: like every worker process, a Messenger consumer's
 * `messenger:consume` run is CLI, and the .so's RINIT hook deliberately skips CLI processes
 * without it — without the flag `NativeExtension::requestStart` is declined and this whole
 * class is a transparent, inert pass-through.
 *
 * Wired into `framework.yaml` under `framework.messenger.buses.<bus>.middleware`, same as any
 * other Messenger middleware service — see the wiring instructions returned alongside this file,
 * since `services.yaml`/`framework.yaml` are integration-owned and not edited here.
 *
 * symfony/messenger is NOT a dependency of this package (the zero-runtime-dependency constraint
 * in composer.json is absolute), so — same technique as ChronosHandler/ChronosLogger next door —
 * the whole class declaration sits behind an `interface_exists()` guard, which is also exactly
 * why this class only ever loads for an application that already depends on Messenger: nothing
 * in this package references the class name unless the app's own Messenger config does.
 */
if (interface_exists(\Symfony\Component\Messenger\Middleware\MiddlewareInterface::class)) {
    final class ChronosMiddleware implements \Symfony\Component\Messenger\Middleware\MiddlewareInterface
    {
        public function handle(
            \Symfony\Component\Messenger\Envelope $envelope,
            \Symfony\Component\Messenger\Middleware\StackInterface $stack,
        ): \Symfony\Component\Messenger\Envelope {
            $received = $envelope->last(\Symfony\Component\Messenger\Stamp\ReceivedStamp::class);

            return $received instanceof \Symfony\Component\Messenger\Stamp\ReceivedStamp
                ? $this->handleConsume($envelope, $stack, $received)
                : $this->handleDispatch($envelope, $stack);
        }

        /**
         * Stamp the child traceparent onto the outgoing envelope BEFORE the send happens — a
         * stamp added after `$stack->next()->handle()` returns would be too late for a real
         * transport, which serializes stamps into the wire message as part of that call — then
         * record the producer span once the send has actually happened, naming the destination
         * from whichever transport `SendMessageMiddleware` sent it to, or falling back to the
         * message's own class when the bus has no routing for it (e.g. a sync-only bus, or a
         * message with no configured transport that only ever reaches an in-process handler).
         */
        private function handleDispatch(
            \Symfony\Component\Messenger\Envelope $envelope,
            \Symfony\Component\Messenger\Middleware\StackInterface $stack,
        ): \Symfony\Component\Messenger\Envelope {
            $traceparent = NativeExtension::childTraceparent();
            if ($traceparent !== null && $traceparent !== '') {
                $envelope = $envelope->with(new ChronosTraceparentStamp($traceparent));
            }

            $result = $stack->next()->handle($envelope, $stack);

            try {
                MessagingSpan::published(
                    'symfony_messenger',
                    self::destinationName($result),
                    $envelope->getMessage()::class,
                );
            } catch (Throwable) {
                // A span that fails to record must never be mistaken for a send that failed.
            }

            return $result;
        }

        /**
         * Open a job-scoped request for the duration of the handler chain (validation, the
         * actual handler, everything downstream of this middleware), continuing the traceparent
         * the dispatch side stamped in, then close it recording success or the exception that
         * escaped.
         */
        private function handleConsume(
            \Symfony\Component\Messenger\Envelope $envelope,
            \Symfony\Component\Messenger\Middleware\StackInterface $stack,
            \Symfony\Component\Messenger\Stamp\ReceivedStamp $received,
        ): \Symfony\Component\Messenger\Envelope {
            $messageClass = $envelope->getMessage()::class;
            $stamp = $envelope->last(ChronosTraceparentStamp::class);
            $traceparent = $stamp instanceof ChronosTraceparentStamp ? $stamp->getTraceparent() : null;

            NativeExtension::requestStart(
                $traceparent,
                null,
                null,
                null,
                null,
                // The queue is this message's "method and route", same vocabulary
                // QueueTelemetry gives a Laravel job: what it was, where it came from.
                'QUEUE',
                $messageClass,
                // Empty service name → native falls back to CHRONOS_PHP_APPLICATION, so a
                // worker and the web requests of the same service land on ONE service map node.
                '',
            );
            if (NativeExtension::active()) {
                NativeExtension::setRequestAttributes(self::consumeAttributes($envelope, $received, $messageClass));
            }

            try {
                $result = $stack->next()->handle($envelope, $stack);
                // Zero, not 200: a consumed message has no HTTP status, and borrowing one
                // would put a number in the column that means something it does not mean.
                NativeExtension::requestEnd(0, $messageClass);

                return $result;
            } catch (Throwable $e) {
                NativeExtension::requestEnd(0, $messageClass, self::unwrap($e), true);
                throw $e;
            }
        }

        /**
         * Root-span attributes describing the message being processed, in the same OTel
         * `messaging.*` vocabulary MessagingSpan uses for the publish side — so the producer
         * and consumer halves of one queue join on the same keys.
         *
         * @return array<string, string>
         */
        private static function consumeAttributes(
            \Symfony\Component\Messenger\Envelope $envelope,
            \Symfony\Component\Messenger\Stamp\ReceivedStamp $received,
            string $messageClass,
        ): array {
            $attributes = [
                'span.kind' => 'consumer',
                'messaging.system' => 'symfony_messenger',
                'messaging.operation' => 'process',
                'messaging.message.name' => $messageClass,
            ];
            try {
                if (method_exists($received, 'getTransportName')) {
                    $transport = $received->getTransportName();
                    if ($transport !== '') {
                        $attributes['messaging.destination.name'] = $transport;
                    }
                }
            } catch (Throwable) {
            }
            // The retry count is the field that separates "slow" from "failing and being
            // retried", which look identical without it.
            $redelivery = $envelope->last(\Symfony\Component\Messenger\Stamp\RedeliveryStamp::class);
            if ($redelivery instanceof \Symfony\Component\Messenger\Stamp\RedeliveryStamp) {
                try {
                    $attributes['messaging.message.retry_count'] = (string) $redelivery->getRetryCount();
                } catch (Throwable) {
                }
            }

            return $attributes;
        }

        /**
         * The transport a message actually went to, read off the `SentStamp`
         * `SendMessageMiddleware` adds once the send has happened — not before, because until
         * then no transport has been chosen. Falls back to the message's own class when the
         * bus never routed it to a transport at all (sync-only handling).
         */
        private static function destinationName(\Symfony\Component\Messenger\Envelope $envelope): string
        {
            try {
                $sent = $envelope->last(\Symfony\Component\Messenger\Stamp\SentStamp::class);
                if ($sent instanceof \Symfony\Component\Messenger\Stamp\SentStamp) {
                    $alias = method_exists($sent, 'getSenderAlias') ? $sent->getSenderAlias() : null;
                    if (is_string($alias) && $alias !== '') {
                        return $alias;
                    }
                    if (method_exists($sent, 'getSenderClass')) {
                        $class = $sent->getSenderClass();
                        if (is_string($class) && $class !== '') {
                            return $class;
                        }
                    }
                }
            } catch (Throwable) {
            }

            return $envelope->getMessage()::class;
        }

        /**
         * By the time an exception reaches this middleware, `HandleMessageMiddleware` further
         * down the stack has already wrapped whatever the handler actually threw in a
         * `HandlerFailedException` — recording that wrapper as the error would show every
         * failing message as the same exception type. The real cause is unwrapped for the span;
         * the wrapper is still what gets rethrown to the caller unchanged.
         */
        private static function unwrap(Throwable $e): Throwable
        {
            if (
                $e instanceof \Symfony\Component\Messenger\Exception\HandlerFailedException
                && method_exists($e, 'getExceptions')
            ) {
                $inner = $e->getExceptions();
                $first = is_array($inner) ? reset($inner) : false;
                if ($first instanceof Throwable) {
                    return $first;
                }
            }

            return $e;
        }
    }
}
