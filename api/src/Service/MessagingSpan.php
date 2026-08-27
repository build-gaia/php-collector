<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Throwable;

/**
 * The span an outbound message gets when it actually leaves the process.
 *
 * ADR 0024 §2 splits messaging into two tiers, and this is the second one. The
 * first — the bounded `messaging.events` / `messaging.jobs` catalog on the
 * request root — answers "what did this request set in motion" cheaply, for
 * everything, including the synchronous listener calls that never touch a broker.
 * A span per one of those would be the flood the catalog exists to avoid.
 *
 * A message that crosses a process boundary is different in kind. It has a
 * destination another service reads from, so it is an EDGE in the topology, and
 * the Data Sources producer graph joins spans to streams on
 * `messaging.destination.name` with the direction read off `messaging.operation`.
 * Those keys have been the assumed contract there since ADR 0023 §6 with nothing
 * emitting them, which is why the producer half of that graph could not be drawn
 * from application telemetry at all.
 *
 * Framework-agnostic on purpose: Laravel broadcasting is the first caller, and a
 * Symfony Messenger transport or a raw Kafka producer wants the same span.
 */
final class MessagingSpan
{
    /**
     * Record one published message.
     *
     * Zero-duration by construction: this is called after the publish, from an
     * event the framework raises once it has happened, so there is no interval to
     * measure — only the fact, the destination and the call site. A span with a
     * real duration would be a claim about timing that was never observed.
     *
     * @param array<string, string> $extra additional `messaging.*` attributes
     */
    public static function published(
        string $system,
        string $destination,
        string $messageName,
        array $extra = [],
    ): void {
        try {
            if ($system === '') {
                return;
            }
            // "publish <destination>" rather than "publish <class>": the span name
            // is what a trace list groups by, and the destination is the shared
            // identity two services see, where the class name is one side's.
            $label = $destination === '' ? $system : $destination;
            $span = SpanManager::open('PUBLISH '.$label);
            if ($span->isVoid()) {
                $span->finish();

                return;
            }
            $span->add('span.kind', 'producer');
            $span->add('messaging.system', $system);
            $span->add('messaging.operation', 'publish');
            if ($destination !== '') {
                $span->add('messaging.destination.name', $destination);
            }
            if ($messageName !== '') {
                $span->add('messaging.message.name', $messageName);
            }
            foreach (CallSite::attributes() as $key => $value) {
                $span->add($key, $value);
            }
            foreach ($extra as $key => $value) {
                if ($value !== '') {
                    $span->add($key, $value);
                }
            }
            $span->finish();
        } catch (Throwable) {
        }
    }
}
