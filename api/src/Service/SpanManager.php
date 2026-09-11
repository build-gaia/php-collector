<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Chronos\Collector\Dto\SpanRecord;
use Chronos\Collector\Dto\SpanReservation;
use Throwable;

/**
 * The $chronos->span object handed to application and auto-instrumentation code. Every
 * SpanManager instance is a stateless handle: the request-scoped span stack lives in static
 * properties, mirroring RichTelemetryContext, so a container singleton created once at boot
 * still behaves correctly per request once the framework integration calls begin()/drains via
 * LocalSpanRecorder::write(). Capacity and disabled states fail open through Span::null().
 */
final class SpanManager
{
    private const MAX_SPANS = 64;

    /** @var list<Span> */
    private static array $stack = [];

    /** @var list<SpanRecord> */
    private static array $finished = [];

    /**
     * Span ids handed out by [`reserve`] and not yet spent by [`openReserved`].
     *
     * It counts toward MAX_SPANS exactly as an open or finished span does, which
     * is the whole point: re-checking the cap when the span is finally RECORDED
     * could refuse a span whose id is already on the wire, recreating the orphan
     * this mechanism exists to remove. Holding the slot at reservation time keeps
     * the 64-span ceiling honest and guarantees that an id which shipped is an id
     * that gets recorded.
     */

    public static function begin(Span $root): void
    {
        self::$stack = [$root];
        self::$finished = [];
    }

    /**
     * Clear request-scoped state. Called from NativeExtension::requestStart because
     * these statics persist across requests inside one FPM worker — without a reset
     * the lazily-seeded synthetic root would carry the previous request's trace ids.
     */
    public static function reset(): void
    {
        self::$stack = [];
        self::$finished = [];
    }

    /** @return list<SpanRecord> */
    public static function end(): array
    {
        $finished = self::$finished;
        self::$stack = [];
        self::$finished = [];
        // A reservation that outlived its request was never recorded. Releasing
        // the slot here is what stops one leaked publish (a send that threw, a
        // Laravel payload the driver rewrote) from permanently shrinking the next
        // request's span budget inside the same FPM worker.

        return $finished;
    }

    public function create(string $name): Span
    {
        return self::open($name);
    }

    public static function open(string $name): Span
    {
        return self::spawn($name, self::top());
    }

    /**
     * Open a span parented on the current top WITHOUT leaving it on the stack.
     *
     * For spans whose lifetime is decoupled from lexical scope — Symfony's lazy HTTP
     * client opens a span at request() time and only closes it when (if!) the caller
     * resolves the response, arbitrarily later. Left on the stack, such a span would
     * become the parent of every span opened in between (SQL, cache, a second
     * concurrent request), and one that is never resolved would mis-parent the rest
     * of the request. Detached, it keeps its own correct parent, everything opened
     * after it parents onto that same parent, and an unresolved one simply never
     * records — a missing span, never a wrong tree. complete() is filter-based, so
     * finishing a detached span works unchanged.
     */
    public static function openDetached(string $name): Span
    {
        $span = self::spawn($name, self::top());
        self::$stack = array_values(array_filter(
            self::$stack,
            static fn (Span $open): bool => $open !== $span,
        ));

        return $span;
    }

    public static function spawn(string $name, ?Span $parent): Span
    {
        if ($parent === null || $parent->isVoid() || count(self::$stack) + count(self::$finished) >= self::MAX_SPANS) {
            return Span::null();
        }
        $child = Span::open($parent->traceId, TraceContext::newSpanId(), $parent->id, $name);
        self::$stack[] = $child;

        return $child;
    }

    /**
     * Reserve the id a span will be recorded under, before that span exists.
     *
     * The messaging bridges need this because the two halves of a publish happen
     * at different instants and in that order: the trace context must be on the
     * wire BEFORE the send, and the publish span must be recorded strictly AFTER
     * it (`MessagingSpan::published()` reports a send that already happened, and
     * recording it first would claim a publish that had not). So the ID is
     * allocated here, put on the wire, and spent by [`openReserved`] once the
     * send has returned.
     *
     * Minted in PHP, with NO new native function, for two reasons. The .so is
     * deployed independently of the composer package — baked into images,
     * installed per host — so a fix needing a new `chronos_*` function would be
     * gated on rebuilding and redeploying the extension across an estate, for a
     * defect that is entirely PHP-side. And it is not expressible natively
     * anyway: the publish span's parent is the top of THIS stack, which the
     * native observer's own span stack knows nothing about.
     *
     * Returns null in exactly the cases where no span could have been recorded —
     * no request open (so `top()` cannot even seed from the native context), a
     * void top, or the span budget spent. A null answer is a signal to the
     * caller, not a failure: it means propagate the request root's traceparent
     * or nothing, never a fabricated id.
     */
    public static function reserve(): ?SpanReservation
    {
        $top = self::top();
        if ($top === null || $top->isVoid()) {
            return null;
        }
        // The cap is checked HERE and nowhere else, and no slot is held.
        //
        // A held slot leaked: only openReserved() released it, and a reservation
        // is legitimately abandoned on several paths — RequestFacts cannot
        // recover the id from a payload over its decode ceiling, from a tier-2
        // stamp, or from any decode failure, and MessagingSpan then takes the
        // plain open() branch. A request dispatching many such jobs exhausted the
        // counter and silently lost publish spans for the rest of itself, which
        // is the failure the reservation exists to prevent.
        //
        // Checking only here is the right trade: refusing at reserve() time
        // means no id goes on the wire, so the caller falls back to the request
        // root's traceparent — a real parent, one step less precise. Refusing at
        // record time would strand an id that had already shipped. The window
        // between the two calls can overshoot MAX_SPANS by the number of
        // concurrent reservations, which is a soft guard overshooting slightly
        // rather than a budget that stops working.
        if (count(self::$stack) + count(self::$finished) >= self::MAX_SPANS) {
            return null;
        }

        return new SpanReservation($top->traceId, TraceContext::newSpanId(), self::sampled());
    }

    /**
     * Open the span a reservation promised, under the id that already went out.
     *
     * DETACHED, never pushed onto the stack, for the reason [`openDetached`]
     * gives at greater length: a publish span is zero-duration and finishes
     * immediately, and a pushed span that never finished would re-parent
     * everything opened after it.
     *
     * Constructed UNCONDITIONALLY — the cap was already paid at reservation time
     * — because the alternative is to refuse a span whose id is on the wire,
     * which is the orphan this whole mechanism removes. The parent is resolved
     * afresh from the current top rather than remembered: a reservation carries
     * an identity, not a position, and the enclosing span is whatever is open
     * when the publish is finally described.
     */
    public static function openReserved(SpanReservation $reservation, string $name): Span
    {
        return Span::open(
            $reservation->traceId,
            $reservation->spanId,
            self::top()?->id ?? '',
            $name,
        );
    }

    /**
     * The reservation shape of a span that is ALREADY open.
     *
     * For the one HTTP site that records its own client span and must propagate
     * that span's real id (Laravel's Http-facade hook): there is nothing to
     * reserve there, the span exists, but the traceparent still has to be built
     * with the request's real sampled flag rather than a hardcoded `01`. Reusing
     * this type keeps one spelling of the header in the SDK instead of two.
     */
    public static function reservationOf(Span $span): SpanReservation
    {
        return new SpanReservation($span->traceId, $span->id, self::sampled());
    }

    /**
     * This request's sampling decision, as the flag that goes on the wire.
     *
     * Read from the native traceparent's last segment, because that string is the
     * one place the two facts PHP cannot mint itself already live — it is exactly
     * what [`seedFromNative`] parses for the trace id. The pure-PHP path falls
     * back to the ambient context, and a request with neither is treated as
     * sampled: propagating `00` for a request whose decision is unknown would
     * silence a downstream service that was recording perfectly well.
     */
    private static function sampled(): bool
    {
        $traceparent = NativeExtension::traceparent();
        if (is_string($traceparent)) {
            $parts = explode('-', $traceparent);
            if (count($parts) === 4 && strlen($parts[3]) === 2 && ctype_xdigit($parts[3])) {
                return (hexdec($parts[3]) & 1) === 1;
            }
        }

        return TraceContext::ambient()?->sampled ?? true;
    }

    public static function complete(Span $span): void
    {
        self::$stack = array_values(array_filter(self::$stack, static fn (Span $open): bool => $open !== $span));
        $record = $span->toRecord();
        // With the native extension loaded the .so owns the span batch: bridge the
        // finished span across the FFI so it ships in the same .trace envelope as
        // the observer spans. The legacy static buffer only backs the pure-PHP path.
        if (NativeExtension::loaded()) {
            NativeExtension::recordSpan($record);

            return;
        }
        if (count(self::$finished) < self::MAX_SPANS) {
            self::$finished[] = $record;
        }
    }

    public static function active(): ?Span
    {
        return self::top();
    }

    /**
     * Record an OTel span event on whichever span is currently open (the top of the request-scoped
     * stack). A no-op when nothing is open, so instrumentation can annotate the active span without
     * threading a handle. Fail-open through Span::recordEvent().
     *
     * @param array<mixed> $attributes
     */
    public static function recordEvent(string $name, array $attributes = [], ?string $timeUnixNano = null): void
    {
        self::top()?->recordEvent($name, $attributes, $timeUnixNano);
    }

    /**
     * Record an OTel span link on whichever span is currently open. A no-op when nothing is open.
     *
     * @param array<mixed> $attributes
     */
    public static function recordLink(string $traceId, string $spanId, array $attributes = []): void
    {
        self::top()?->recordLink($traceId, $spanId, $attributes);
    }

    /**
     * Userland call-through for a decorated method. This is the stopgap that makes Chronos\
     * trace_method() do something before the native extension exists: an application (or its
     * instrumentation manifest) that cannot rely on transparent zend interception routes the call
     * itself through here. When a decoration is registered for "$class::$method" a child span is
     * spawned on entry (reusing the null-safe stack, so a disabled or capacity-bound request degrades
     * to a plain call), the decorator is invoked to shape that span, the original callable runs, and
     * the span is completed on exit even if the callable throws. With no decoration registered this is
     * a transparent pass-through with zero span overhead.
     *
     * Transparent, no-code-change interception of arbitrary methods arrives with the native .so (see
     * native/php/): that build wires zend_observer_fcall_register to the SAME SpanDecorations registry,
     * so manifests written against this API keep working unchanged once the extension is installed.
     *
     * @param array<int, mixed> $arguments
     */
    public static function callThrough(string $class, string $method, callable $original, array $arguments = []): mixed
    {
        $decorator = SpanDecorations::lookup($class, $method);
        if ($decorator === null) {
            return $original(...$arguments);
        }
        $span = self::open($class.'::'.$method);
        try {
            $decorator($span, $arguments);
        } catch (Throwable) {
            // A misbehaving decorator must never break the call it was only meant to observe.
        }
        try {
            return $original(...$arguments);
        } finally {
            $span->finish();
        }
    }

    private static function top(): ?Span
    {
        if (self::$stack === []) {
            self::seedFromNative();
        }
        $top = end(self::$stack);

        return $top === false ? null : $top;
    }

    /**
     * With the native extension driving the request lifecycle, nothing calls begin() —
     * historically that left top() null forever, so every $chronos->span->create() and
     * Doctrine SQL span silently no-oped. Seed the stack lazily from the native trace
     * context instead: a synthetic root bound to the request's trace/span ids that acts
     * purely as a parent (it is never finished, so it never emits a duplicate root —
     * the .so writes the real request root span at request end).
     */
    private static function seedFromNative(): void
    {
        if (!NativeExtension::loaded()) {
            return;
        }
        $traceparent = NativeExtension::traceparent();
        if ($traceparent === null) {
            return;
        }
        $parts = explode('-', $traceparent);
        if (count($parts) !== 4 || strlen($parts[1]) !== 32 || strlen($parts[2]) !== 16) {
            return;
        }
        self::$stack = [Span::open($parts[1], $parts[2], '', 'request')];
        self::$finished = [];
    }

    /**
     * Closes a "bootstrap" span (built with Span::open() directly, so it was never pushed onto
     * the active stack and never intercepted another span's parenting) at the start time of
     * whichever already-finished child span began first. This turns framework-boot dead time
     * before the first instrumented hook into its own timed span. Falls back to "now" when no
     * child span finished during the request, so an entirely uninstrumented request reports a
     * full-width bootstrap span rather than a zero-width one.
     */
    public static function finishBootstrap(Span $bootstrap): void
    {
        $earliest = null;
        foreach (self::$finished as $child) {
            if ($earliest === null || $child->startedAt < $earliest) {
                $earliest = $child->startedAt;
            }
        }
        $bootstrap->finishAt($earliest);
    }
}
