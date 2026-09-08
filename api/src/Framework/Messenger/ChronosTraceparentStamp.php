<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Messenger;

/**
 * Carries the child traceparent that continues a dispatching request's trace into the
 * consumer half of a Symfony Messenger message.
 *
 * Same shape as the `chronos` key QueueTelemetry stamps into a Laravel job's payload, and for
 * the same reason: a Messenger envelope is Symfony's own supported place to ride extra context
 * ALONGSIDE the message, so this survives whatever the transport's own serializer does to the
 * envelope (Doctrine, AMQP, Redis, in-memory — all of them carry stamps unless a stamp opts out
 * via `NonSendableStampInterface`, which this deliberately does not: the whole point is that it
 * DOES travel to the worker). A header a particular transport happens to preserve would only
 * work for that one transport; a stamp is transport-agnostic by construction.
 *
 * A CHILD traceparent, not the dispatching request's own: the consumed message is caused by the
 * request that dispatched it but is not part of it, so the job hangs beneath the dispatch as a
 * new trace rooted there rather than claiming to BE the same span — the same shape an outbound
 * HTTP call or a Laravel queued job gets.
 *
 * symfony/messenger is NOT a dependency of this package (the zero-runtime-dependency constraint
 * in composer.json is absolute), so — same technique as ChronosHandler/ChronosLogger next door —
 * the whole class declaration sits behind an `interface_exists()` guard: PHP only resolves an
 * `implements` target when the class declaration statement EXECUTES, so a false condition here
 * skips declaring the class entirely and this file loads cleanly whether or not Messenger is on
 * the app's classmap.
 */
if (interface_exists(\Symfony\Component\Messenger\Stamp\StampInterface::class)) {
    final class ChronosTraceparentStamp implements \Symfony\Component\Messenger\Stamp\StampInterface
    {
        public function __construct(private readonly string $traceparent)
        {
        }

        public function getTraceparent(): string
        {
            return $this->traceparent;
        }
    }
}
