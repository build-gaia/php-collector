<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Messenger;

use Chronos\Collector\Service\MessagingSpan;
use Chronos\Collector\Service\NativeExtension;
use Throwable;

/**
 * The two halves of a Symfony Messenger message, joined into one trace — the Messenger
 * counterpart of Laravel's QueueTelemetry, for the same underlying reason: a message that
 * leaves the process, whether over AMQP, Doctrine or Redis, is work the dispatching request
 * caused, and a trace that stops at dispatch hides all of it. (The sync transport is the
 * deliberate exception on BOTH halves: its messages never leave the process, so its inline
 * handling belongs to the enclosing request's trace, not to a fabricated queue.)
 *
 * One middleware, two lifecycles, told apart by `ReceivedStamp`'s presence — Messenger's own
 * signal that an envelope arrived from a transport's `receive()` rather than from `dispatch()`:
 *
 *   DISPATCH (no ReceivedStamp): a producer span, the same `MessagingSpan` used for Laravel
 *   broadcasting and any other publish, so the two halves of a queue join the Data Sources
 *   producer graph on the same `messaging.*` keys regardless of framework. Recorded AFTER
 *   `$stack->next()->handle()` returns, not before — same reason MessagingSpan is zero-duration
 *   by construction: this fires from the fact that the send already happened, and only the
 *   send itself (via `SendMessageMiddleware`, downstream of this one in the default bus) knows
 *   which transport the message actually went to — and only when a `SentStamp` proves a
 *   transport DID accept it: a dispatch the bus handled synchronously in-process is a function
 *   call, not a topology edge, and gets no producer span (see handleDispatch).
 *
 *   CONSUME (ReceivedStamp present, and not the sync transport's inline re-dispatch — see
 *   handleConsume's guard): a job-scoped request, continuing the traceparent this middleware
 *   stamped onto the envelope on the way out. Same trade as QueueTelemetry: the worker's
 *   HANDLE call is treated as the request root ('QUEUE' / the message class stand in for
 *   method / route) so everything already built on the request root — facts, DST recording,
 *   the cache suppression the HTTP path declares (re-declared here, since suppression is
 *   per-request) — works inside a consumed message, rather than inventing a parallel
 *   lifecycle for workers.
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
         * from whichever transport `SendMessageMiddleware` sent it to.
         *
         * ONLY when a send happened: `SentStamp` is the proof a transport accepted the
         * message, and without it the whole downstream chain — including the handler itself —
         * ran synchronously in-process. That is a function call, not a topology edge, and a
         * producer span for it would fabricate a Data Sources producer-graph edge (the graph
         * joins on messaging.destination.name/messaging.operation) to a stream that does not
         * exist — one per dispatch, for every CQRS-style sync command bus.
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
                $destination = self::destinationName($result);
                if ($destination !== null) {
                    MessagingSpan::published(
                        'symfony_messenger',
                        $destination,
                        $envelope->getMessage()::class,
                    );
                }
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
            // A ReceivedStamp does NOT always mean a worker: SyncTransport re-runs the bus
            // with `ReceivedStamp('sync')` inside whatever request dispatched the message,
            // and requestStart/requestEnd here would hijack that live request — reset its
            // open span stack, rewrite method/route to QUEUE/<class>, and close the WHOLE
            // request root early (chronos_request_end is first-call-wins, so the kernel's
            // real end would then no-op and everything after the dispatch goes untraced).
            // Both guards are needed: the transport name catches the sync transport even
            // before any request opened, and the open-request probe catches every other
            // synchronous re-dispatch shape (whatever the transport was named). Inline
            // handling is already traced as part of the enclosing request, so pass through.
            if (self::consumesInline($received) || NativeExtension::active()) {
                return $stack->next()->handle($envelope, $stack);
            }

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
                // Same stand-down ChronosHttpKernel declares for a web request: the
                // decorated cache pools (ChronosCachePass) own cache spans in a worker
                // too, so the native Redis/Memcached fallback must not double-emit.
                // SQL needs no equivalent here — DoctrineQuerySpan declares its own
                // suppression at the moment it actually instruments a query.
                NativeExtension::suppressNative('cache');
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
         * Whether this "received" envelope is really being handled INLINE by the sync
         * transport rather than by a worker. SyncTransport hardcodes 'sync' as the
         * transport name on the ReceivedStamp it re-dispatches with, whatever the
         * application called the transport in messenger.yaml.
         */
        private static function consumesInline(\Symfony\Component\Messenger\Stamp\ReceivedStamp $received): bool
        {
            try {
                return method_exists($received, 'getTransportName')
                    && $received->getTransportName() === 'sync';
            } catch (Throwable) {
                return false;
            }
        }

        /**
         * The transport a message actually went to, read off the `SentStamp`
         * `SendMessageMiddleware` adds once the send has happened — not before, because until
         * then no transport has been chosen. Null when the bus never routed the message to a
         * transport at all (sync-only handling, or a class with no configured routing): no
         * SentStamp means nothing left the process, and handleDispatch must record no
         * producer span for it. The message's own class is only a last-resort NAME for a
         * stamp whose alias and sender class are both blank — never a substitute for the
         * stamp's presence.
         */
        private static function destinationName(\Symfony\Component\Messenger\Envelope $envelope): ?string
        {
            try {
                $sent = $envelope->last(\Symfony\Component\Messenger\Stamp\SentStamp::class);
                if (!$sent instanceof \Symfony\Component\Messenger\Stamp\SentStamp) {
                    return null;
                }
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
            } catch (Throwable) {
                return null;
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
