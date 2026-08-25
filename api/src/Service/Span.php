<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Chronos\Collector\Dto\SpanRecord;
use DateTimeImmutable;
use DateTimeZone;
use Throwable;

/**
 * A single open span handle, returned by $chronos->span->create()/child(). The void flag makes
 * every method a safe no-op so callers never need to check whether tracing is active: a
 * disabled collector or a capacity-bound stack hands back the same inert Span as a live one.
 */
final class Span
{
    private const MAX_ATTRIBUTES = 32;
    private const MAX_KEY_LENGTH = 128;
    private const MAX_VALUE_LENGTH = 512;
    /**
     * A far larger ceiling for a handful of attributes whose whole point is the full text —
     * chiefly db.statement (and its bound-parameter JSON). The generic 512 cap keeps arbitrary
     * attributes bounded, but a truncated SQL statement is worse than useless in the UI, so
     * capture the whole query up to a still-bounded 16 KiB.
     */
    public const MAX_TEXT_LENGTH = 16384;
    private const MAX_NAME_LENGTH = 128;
    private const MAX_PARAMETERS = 32;
    private const MAX_PARAMETER_LENGTH = 64;

    /**
     * OTel span-model bounds. Events and links are recorded ADDITIVELY and emitted as JSON-encoded
     * span attributes (span.events / span.links), so the existing span-batch envelope and engine
     * attribute passthrough carry them with no engine change. Every recording path is fail-open:
     * a broken event or link can never break the span it was only meant to annotate.
     */
    private const MAX_EVENTS = 128;
    private const MAX_LINKS = 32;
    private const MAX_EVENT_ATTRIBUTES = 32;

    private string $startedAt;
    private ?string $endedAtOverride = null;
    private bool $finished = false;
    private string $status = 'ok';

    /** @var array<string, string> */
    private array $attributes = [];

    /** @var list<array{name: string, timeUnixNano: string, attributes: array<string, string>}> */
    private array $events = [];

    /** @var list<array{traceId: string, spanId: string, attributes: array<string, string>}> */
    private array $links = [];

    private function __construct(
        public readonly string $traceId,
        public readonly string $id,
        public string $parentSpanId,
        public readonly string $name,
        private readonly bool $void,
    ) {
        $this->startedAt = self::now();
    }

    public static function open(string $traceId, string $id, string $parentSpanId, string $name): self
    {
        $safeName = trim($name) === '' ? 'span' : self::cap($name, self::MAX_NAME_LENGTH);

        return new self($traceId, $id, $parentSpanId, $safeName, false);
    }

    public static function null(): self
    {
        return new self('', '', '', '', true);
    }

    public function isVoid(): bool
    {
        return $this->void;
    }

    /**
     * $maxLength lets a caller opt a specific attribute into the larger MAX_TEXT_LENGTH ceiling
     * (e.g. db.statement); it defaults to the generic MAX_VALUE_LENGTH bound and is itself clamped
     * to MAX_TEXT_LENGTH so no caller can request an unbounded value.
     */
    public function add(string $key, mixed $value, int $maxLength = self::MAX_VALUE_LENGTH): void
    {
        if ($this->void || $this->finished) {
            return;
        }
        $safeKey = self::cap(trim($key), self::MAX_KEY_LENGTH);
        if ($safeKey === '') {
            return;
        }
        if (!array_key_exists($safeKey, $this->attributes) && count($this->attributes) >= self::MAX_ATTRIBUTES) {
            return;
        }
        $this->attributes[$safeKey] = self::cap(self::stringify($value), min(max($maxLength, 0), self::MAX_TEXT_LENGTH));
    }

    /**
     * Record an OTel span EVENT: a timestamped, named point-in-time annotation with its own
     * attributes (e.g. an "exception" event, a cache miss, a retry). Bounded to MAX_EVENTS;
     * attributes are redaction-gated and bounded. $timeUnixNano defaults to "now"; a caller may
     * supply an already-observed nanosecond timestamp. Fail-open: a broken event is dropped, never
     * thrown, and never disturbs the span.
     *
     * @param array<mixed> $attributes
     */
    public function recordEvent(string $name, array $attributes = [], ?string $timeUnixNano = null): void
    {
        if ($this->void || $this->finished) {
            return;
        }
        try {
            if (count($this->events) >= self::MAX_EVENTS) {
                return;
            }
            $safeName = trim($name) === '' ? 'event' : self::cap(trim($name), self::MAX_NAME_LENGTH);
            $this->events[] = [
                'name' => $safeName,
                'timeUnixNano' => self::normaliseTime($timeUnixNano),
                'attributes' => self::sanitiseAttributes($attributes),
            ];
        } catch (Throwable) {
            // A broken event must never break the span it was only meant to annotate.
        }
    }

    /**
     * Record an OTel span LINK: a reference to a causally-related span in this or another trace,
     * with its own attributes (e.g. a follows-from relationship, a batched-message producer span).
     * Bounded to MAX_LINKS; attributes are redaction-gated and bounded. Fail-open.
     *
     * @param array<mixed> $attributes
     */
    public function recordLink(string $traceId, string $spanId, array $attributes = []): void
    {
        if ($this->void || $this->finished) {
            return;
        }
        try {
            if (count($this->links) >= self::MAX_LINKS) {
                return;
            }
            $safeTraceId = self::cap(trim($traceId), self::MAX_NAME_LENGTH);
            $safeSpanId = self::cap(trim($spanId), self::MAX_NAME_LENGTH);
            if ($safeTraceId === '' || $safeSpanId === '') {
                return;
            }
            $this->links[] = [
                'traceId' => $safeTraceId,
                'spanId' => $safeSpanId,
                'attributes' => self::sanitiseAttributes($attributes),
            ];
        } catch (Throwable) {
            // A broken link must never break the span it was only meant to annotate.
        }
    }

    /**
     * Auto-capture convenience: record a caught throwable as the OTel-conventional "exception" span
     * event, carrying exception.type, exception.stacktrace and (rich-telemetry only, since it can
     * contain user data) exception.message. This is ADDITIVE to any legacy exception.* attributes a
     * caller also sets; both coexist.
     */
    /**
     * Mark this span errored; toRecord() then emits status "error" so the engine's
     * span index and trace views surface the failure.
     */
    public function markError(): void
    {
        if ($this->void || $this->finished) {
            return;
        }
        $this->status = 'error';
    }

    public function recordException(Throwable $exception, bool $rich, ?string $stacktraceJson = null): void
    {
        $this->markError();
        $attributes = ['exception.type' => get_class($exception)];
        if ($rich) {
            $attributes['exception.message'] = $exception->getMessage();
        }
        if ($stacktraceJson !== null) {
            $attributes['exception.stacktrace'] = $stacktraceJson;
        }
        $attributes['code.filepath'] = $exception->getFile();
        $attributes['code.lineno'] = (string) $exception->getLine();
        $this->recordEvent('exception', $attributes);
    }

    public function child(string $name): self
    {
        if ($this->void) {
            return self::null();
        }

        return SpanManager::spawn($name, $this);
    }

    public function nest(self $child): void
    {
        if ($this->void || $child->void) {
            return;
        }
        $child->parentSpanId = $this->id;
    }

    public function finish(): void
    {
        if ($this->void || $this->finished) {
            return;
        }
        $this->finished = true;
        SpanManager::complete($this);
    }

    /**
     * Lets bootstrap-span accounting close a span at an already-observed timestamp (the start
     * of the first real child span) instead of "now", so dead time before the first
     * instrumented hook is measured exactly rather than approximated at request end.
     */
    public function finishAt(?string $timestamp): void
    {
        if ($this->void || $this->finished) {
            return;
        }
        if ($timestamp !== null) {
            $this->endedAtOverride = $timestamp;
        }
        $this->finish();
    }

    /**
     * Bounds a bound-parameter list into a short JSON array attribute value: at most
     * MAX_PARAMETERS entries, each stringified and capped to MAX_PARAMETER_LENGTH before
     * encoding, so a query with many or huge bindings can never turn into an unbounded
     * payload. add() further caps the encoded result to MAX_VALUE_LENGTH.
     */
    public static function boundedParametersJson(array $values): string
    {
        $capped = array_map(
            static fn (mixed $value): string => self::cap(self::stringify($value), self::MAX_PARAMETER_LENGTH),
            array_slice(array_values($values), 0, self::MAX_PARAMETERS),
        );
        $encoded = json_encode($capped, JSON_UNESCAPED_SLASHES);

        return is_string($encoded) ? $encoded : '[]';
    }

    /**
     * Internal: lets auto-instrumentation that only observes a query after it already ran
     * (Laravel's DB::listen fires post-execution with an elapsed duration) set a real start
     * time instead of the construction-time timestamp.
     */
    public function backdateStart(float $millisecondsAgo): void
    {
        if ($this->void || $this->finished || $millisecondsAgo <= 0) {
            return;
        }
        $timestamp = microtime(true) - ($millisecondsAgo / 1000);
        $backdated = DateTimeImmutable::createFromFormat('U.u', sprintf('%.6F', $timestamp));
        if ($backdated === false) {
            return;
        }
        $this->startedAt = $backdated->setTimezone(new DateTimeZone('UTC'))->format('Y-m-d\TH:i:s.u\Z');
    }

    public function toRecord(): SpanRecord
    {
        $attributes = $this->attributes;
        self::attachStructured($attributes, 'span.events', $this->events);
        self::attachStructured($attributes, 'span.links', $this->links);

        return new SpanRecord($this->traceId, $this->id, $this->parentSpanId, $this->name, $this->startedAt, $this->endedAtOverride ?? self::now(), $this->status, $attributes);
    }

    /**
     * JSON-encode a bounded event/link collection onto a span attribute, mirroring how
     * exception.stacktrace is JSON-encoded (span attribute values are strings in the
     * chronos.tracing.span-batch.v1 envelope). Fail-open: an unencodable collection is skipped,
     * never thrown, so the span is always written.
     *
     * @param array<string, string>                  $attributes
     * @param list<array<string, mixed>>             $items
     */
    private static function attachStructured(array &$attributes, string $key, array $items): void
    {
        if ($items === []) {
            return;
        }
        try {
            $attributes[$key] = json_encode(array_values($items), JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
        } catch (Throwable) {
            // Fail open: an unencodable events/links payload drops the attribute, never the span.
        }
    }

    /**
     * Bound and redact an event/link attribute bag: at most MAX_EVENT_ATTRIBUTES entries, each key
     * and value capped, and any sensitive-keyed value masked through the canonical redaction policy
     * so credentials never leave the process on an event or link.
     *
     * @param array<mixed> $attributes
     * @return array<string, string>
     */
    private static function sanitiseAttributes(array $attributes): array
    {
        $out = [];
        foreach ($attributes as $key => $value) {
            if (count($out) >= self::MAX_EVENT_ATTRIBUTES) {
                break;
            }
            $safeKey = self::cap(trim((string) $key), self::MAX_KEY_LENGTH);
            if ($safeKey === '') {
                continue;
            }
            $stringValue = self::cap(self::stringify($value), self::MAX_TEXT_LENGTH);
            $out[$safeKey] = self::isSensitiveKey($safeKey) ? '***' : $stringValue;
        }

        return $out;
    }

    private static function isSensitiveKey(string $key): bool
    {
        $lower = strtolower($key);
        return str_contains($lower, 'password') || str_contains($lower, 'secret')
            || str_contains($lower, 'token') || str_contains($lower, 'key')
            || str_contains($lower, 'credential') || str_contains($lower, 'auth');
    }

    /** Accept a caller-supplied nanosecond timestamp only if it is plausible digits; otherwise use "now". */
    private static function normaliseTime(?string $timeUnixNano): string
    {
        if ($timeUnixNano !== null && preg_match('/^[0-9]{1,25}$/D', $timeUnixNano) === 1) {
            return $timeUnixNano;
        }

        return self::nowUnixNano();
    }

    /** Current wall-clock time as a Unix nanosecond string (microsecond resolution, zero-padded). */
    private static function nowUnixNano(): string
    {
        return ((string) (int) (microtime(true) * 1_000_000)).'000';
    }

    private static function stringify(mixed $value): string
    {
        if (is_string($value)) {
            return $value;
        }
        if (is_bool($value)) {
            return $value ? 'true' : 'false';
        }
        if ($value === null) {
            return '';
        }
        if (is_scalar($value)) {
            return (string) $value;
        }
        $encoded = json_encode($value, JSON_UNESCAPED_SLASHES);

        return is_string($encoded) ? $encoded : '';
    }

    private static function cap(string $value, int $max): string
    {
        return strlen($value) > $max ? substr($value, 0, $max) : $value;
    }

    private static function now(): string
    {
        return (new DateTimeImmutable('now', new DateTimeZone('UTC')))->format('Y-m-d\TH:i:s.u\Z');
    }
}
