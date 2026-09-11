<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Chronos\Collector\Dto\SpanReservation;
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
     * The one `$extra` key that is exempted from the generic attribute cap —
     * see the note in [`published`] on why it needs `Span::MAX_TEXT_LENGTH`.
     * Spelled as a literal rather than reused from MessagingBody so that the
     * common publish path (Laravel broadcasting, Messenger) does not load the
     * payload-capture class it never calls.
     */
    public const BODY = 'messaging.message.body';

    /**
     * Record one published message.
     *
     * Zero-duration by construction: this is called after the publish, from an
     * event the framework raises once it has happened, so there is no interval to
     * measure — only the fact, the destination and the call site. A span with a
     * real duration would be a claim about timing that was never observed.
     *
     * The signature is deliberately unchanged as `$extra` grew: `''` still
     * means absent for `$destination` and `$messageName`, so the existing call
     * sites (Laravel broadcasting, Messenger's dispatch half, the request-facts
     * catalog) neither churn nor behave differently. The three parameters after
     * it are trailing and optional for the same reason.
     *
     * ## $reservation — the id that is already on the wire
     *
     * When a caller reserved a span id before the send and propagated it, the
     * span MUST be recorded under that id: the header is a promise, and until
     * this existed every consumer parented itself to an id no span carried, which
     * is why a publish and its consume shared a trace and had no edge between
     * them. Passing the reservation is what closes that loop. Without one the
     * span gets a freshly minted id exactly as before — correct, just not
     * something a consumer can point at.
     *
     * The ordering constraint is satisfied without weakening anything: the id was
     * allocated before the send, and the SPAN is still recorded strictly after
     * it. Nothing here claims a publish that has not happened.
     *
     * ## $wholeBody — the payload the span attribute cannot hold
     *
     * `$extra` already carries the PREVIEW (cut to this span's 16 KiB attribute
     * ceiling by `MessagingBody::encode`). `$wholeBody` is the same payload cut
     * only to the operator's allowance, handed to the span-body store — which is
     * possible here and nowhere else, because this is the only place holding the
     * Span and therefore the only place that knows the span id the blob has to be
     * keyed by. It is stored BEFORE `finish()`, because `finish()` is what ships
     * the record across the FFI, so whether `.stored` may be claimed has to be
     * settled first. `$wholeBodyEncoding` is the transfer encoding
     * (`base64` or `''`) as `MessagingBody::whole()` resolved it.
     *
     * @param array<string, string> $extra additional `messaging.*` attributes
     */
    public static function published(
        string $system,
        string $destination,
        string $messageName,
        array $extra = [],
        ?SpanReservation $reservation = null,
        string $wholeBody = '',
        string $wholeBodyEncoding = '',
    ): void {
        try {
            if ($system === '') {
                return;
            }
            // "publish <destination>" rather than "publish <class>": the span name
            // is what a trace list groups by, and the destination is the shared
            // identity two services see, where the class name is one side's.
            //
            // When there IS no destination the fallback walks outward through the
            // next-most-specific bounded identity rather than jumping straight to
            // $system. A topic publish legitimately has no queue (see
            // MessagingDestination::forAmqp), and naming it "PUBLISH rabbitmq"
            // collapsed every publish in the estate into one row — it named the
            // BROKER rather than the place, and destroyed exactly the
            // discrimination a span name exists to provide. The exchange is the
            // right next step because it is the same string the consumer's own
            // binding names, so the name stays join-compatible; the routing key
            // is only reached when there is no exchange either, i.e. the default
            // exchange, where the key IS the queue. A routing key under a named
            // exchange is deliberately NOT folded in: in a real estate those
            // carry entity ids, and an unbounded span name makes a trace list
            // ungroupable. $system remains the floor so no existing caller can
            // regress.
            $label = $destination;
            if ($label === '') {
                $label = self::text($extra, MessagingDestination::VIA);
            }
            if ($label === '') {
                $label = self::text($extra, MessagingDestination::ROUTE);
            }
            if ($label === '') {
                $label = $system;
            }
            $name = 'PUBLISH '.$label;
            // openReserved() is unconditional by design — the span budget was
            // already paid when the id was reserved — so a reserved publish can
            // never be refused after its id has gone out on the wire.
            $span = $reservation === null
                ? SpanManager::open($name)
                : SpanManager::openReserved($reservation, $name);
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
                if ($value === '') {
                    continue;
                }
                // The body gets the same exemption db.statement has, for the same
                // reason: add()'s default ceiling is Span::MAX_VALUE_LENGTH (512),
                // and a message payload cut at 512 bytes is worse than useless —
                // it is a payload that LOOKS complete. MessagingBody has already
                // truncated to this exact bound and set .truncated if it had to,
                // so nothing here can silently shorten a body again.
                if ($key === self::BODY) {
                    $span->add($key, $value, Span::MAX_TEXT_LENGTH);

                    continue;
                }
                $span->add($key, $value);
            }
            // Last, and before finish(): the marker may only be stamped once the
            // collector has actually TAKEN the bytes, and finish() is what ships
            // the span. A `.stored` attribute that resolves to nothing is the one
            // failure the blob store exists to prevent.
            if ($wholeBody !== '' && NativeExtension::storeSpanBody(
                $span->traceId,
                $span->id,
                'message',
                self::text($extra, 'messaging.message.body.content_type'),
                $wholeBody,
                $wholeBodyEncoding,
            )) {
                $span->add(MessagingBody::STORED, 'true');
            }
            $span->finish();
        } catch (Throwable) {
            // A span that fails to record must never be mistaken for a send
            // that failed: by the time this runs the message has already gone.
        }
    }

    /**
     * One `$extra` entry as a trimmed string, or `''` where there is nothing
     * usable. Typed loosely on purpose — `$extra` is an array a caller built,
     * and a span name is not worth a TypeError.
     *
     * @param array<string, string> $extra
     */
    private static function text(array $extra, string $key): string
    {
        $value = $extra[$key] ?? null;

        return is_scalar($value) ? trim((string) $value) : '';
    }
}
