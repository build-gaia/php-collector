<?php

declare(strict_types=1);

namespace Chronos\Collector\Dto;

/**
 * A span id minted BEFORE the span that will carry it exists.
 *
 * ## The bug this type exists to remove
 *
 * A publish has to put trace context on the wire before the send, and the
 * publish span can only be recorded after it — `MessagingSpan::published()` is
 * zero-duration by construction precisely because it reports a send that has
 * already happened. Something has to bridge those two instants, and until now
 * nothing did: every messaging bridge called
 * `NativeExtension::childTraceparent()`, which mints a fresh span id that NO
 * SPAN IS EVER RECORDED UNDER. The consumer dutifully parented its root to that
 * id, the id named nothing, and so publish and consume shared a trace id and had
 * no parent/child edge at all. On one real trace that was 259 spans with eight
 * orphans — a whole service's work hanging in mid-air.
 *
 * A reservation is the missing bridge. The id is allocated first, propagated,
 * and then USED when the span is finally recorded, so the header's middle field
 * names a span that exists.
 *
 * ## Why the traceparent on the wire is a promise
 *
 * The rule the messaging bridges now follow is that a traceparent is a PROMISE
 * that the span it names will be recorded. That is why this type is handed
 * around rather than a bare string: a caller holding a reservation is holding
 * the obligation to record it, and a caller with no reservation propagates the
 * request root's traceparent (a real span, less precise) or nothing at all
 * (the consumer roots its own trace, which is honest) — never a fabricated id.
 *
 * ## Shape
 *
 * Version `00` only, byte-identical to `TraceContext::header()`, the native
 * `context.rs` `TraceContext::header()` and the format `chronos_child_traceparent`
 * emitted — so native `parse_traceparent` and PHP `TraceContext::fromHeader`
 * both accept it with no change. `sampled` is the PUBLISHER's own flag,
 * unmodified, including `00`: native `start_request` only re-rolls the sampling
 * die when there is no inbound parent, so an unsampled trace is dropped WHOLE
 * rather than half-recorded.
 *
 * Framework-free and dependency-free on purpose (it is a Dto, not a Service): it
 * neither reads the extension nor mints ids, so the standalone suite can prove
 * the wire shape without a .so.
 *
 * Per-property `readonly` rather than a `readonly class`: composer.json requires
 * php >=8.1, and a readonly CLASS is 8.2 — the same reason `SpanRecord` next door
 * spells it this way.
 */
final class SpanReservation
{
    public function __construct(
        public readonly string $traceId,
        public readonly string $spanId,
        public readonly bool $sampled,
    ) {
    }

    /** The W3C traceparent naming the reserved span. */
    public function header(): string
    {
        return '00-'.$this->traceId.'-'.$this->spanId.'-'.($this->sampled ? '01' : '00');
    }

    /**
     * Recover a reservation from a traceparent already on the wire, or null when
     * the header is not one this SDK can honour.
     *
     * The Laravel queue path needs this and nothing else does: its traceparent is
     * stamped during `push` (`Queue::createPayloadUsing`) while the producer span
     * is recorded later, from the `JobQueued` event, in a different call. Rather
     * than parking the reservation in a side channel — which misaligns on
     * `Queue::bulk` or on a push that throws mid-batch, and a misaligned claim
     * would give a producer span ANOTHER message's id, a wrong parent that looks
     * like a working trace — the id is read back out of the payload. The payload
     * IS the wire, so a recovered id cannot be the wrong one.
     *
     * Strict by design, matching `TraceContext::fromHeader`'s own pattern
     * exactly: version `00`, lowercase hex, and an all-zero trace or span id
     * refused (W3C forbids both, and accepting one would put an unusable parent
     * on a span).
     */
    public static function fromTraceparent(string $header): ?self
    {
        if (\preg_match('/^00-([a-f0-9]{32})-([a-f0-9]{16})-([a-f0-9]{2})$/D', \trim($header), $matches) !== 1) {
            return null;
        }
        if ($matches[1] === \str_repeat('0', 32) || $matches[2] === \str_repeat('0', 16)) {
            return null;
        }

        return new self($matches[1], $matches[2], (\hexdec($matches[3]) & 1) === 1);
    }
}
