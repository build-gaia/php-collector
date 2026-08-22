<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Chronos\Collector\Dto\SpanRecord;
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

    public static function spawn(string $name, ?Span $parent): Span
    {
        if ($parent === null || $parent->isVoid() || count(self::$stack) + count(self::$finished) >= self::MAX_SPANS) {
            return Span::null();
        }
        $child = Span::open($parent->traceId, TraceContext::newSpanId(), $parent->id, $name);
        self::$stack[] = $child;

        return $child;
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
