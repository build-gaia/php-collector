<?php
declare(strict_types=1);
namespace Chronos\Collector\Service;
use Chronos\Collector\Dto\DstDirective;
use InvalidArgumentException;

final class TraceContext
{
    /** W3C tracestate: at most 32 list members (https://www.w3.org/TR/trace-context/#tracestate-header). */
    private const MAX_TRACESTATE_ENTRIES = 32;
    /** W3C baggage: recommended >=64 members; total header bounded (https://www.w3.org/TR/baggage/). */
    private const MAX_BAGGAGE_ENTRIES = 64;
    private const MAX_BAGGAGE_HEADER_BYTES = 8192;
    private const MAX_BAGGAGE_VALUE_BYTES = 4096;
    private const MAX_BAGGAGE_KEY_BYTES = 256;

    /**
     * @param array<string,string>|null $tracestate ordered vendor state (insertion order preserved), null when absent
     * @param array<string,string>|null $baggage    ordered key => percent-decoded value, null when absent
     */
    private function __construct(
        public readonly string $traceId,
        public readonly string $spanId,
        public readonly ?string $parentSpanId,
        public readonly bool $sampled,
        public readonly ?string $sessionId = null,
        public readonly ?DstDirective $dstDirective = null,
        public readonly ?array $tracestate = null,
        public readonly ?array $baggage = null,
    ) {
    }

    public static function fromHeader(?string $header, ?string $sessionHeader = null, ?string $dstHeader = null, ?string $tracestateHeader = null, ?string $baggageHeader = null): self
    {
        $session = is_string($sessionHeader) && trim($sessionHeader) !== '' ? trim($sessionHeader) : null;
        $dst = DstDirective::parse($dstHeader);
        $tracestate = self::parseTracestate($tracestateHeader);
        $baggage = self::parseBaggage($baggageHeader);
        if ($header === null || trim($header)==='') return new self(bin2hex(random_bytes(16)),self::newSpanId(),null,true,$session,$dst,$tracestate,$baggage);
        if (preg_match('/^00-([a-f0-9]{32})-([a-f0-9]{16})-([a-f0-9]{2})$/D',$header,$m)!==1 || $m[1]===str_repeat('0',32)||$m[2]===str_repeat('0',16)) throw new InvalidArgumentException('Invalid traceparent');
        return new self($m[1],self::newSpanId(),$m[2],(hexdec($m[3])&1)===1,$session,$dst,$tracestate,$baggage);
    }

    /**
     * Returns a copy carrying an explicit sampled flag, preserving every other field. The sampling
     * gate uses this to stamp a root request's local sampling decision onto the context it will
     * propagate downstream, without re-parsing or re-minting ids.
     */
    public function withSampled(bool $sampled): self
    {
        return new self($this->traceId, $this->spanId, $this->parentSpanId, $sampled, $this->sessionId, $this->dstDirective, $this->tracestate, $this->baggage);
    }

    public static function newSpanId(): string { return bin2hex(random_bytes(8)); }
    public function header(): string { return '00-'.$this->traceId.'-'.$this->spanId.'-'.($this->sampled?'01':'00'); }

    /**
     * The request-scoped "current" context, mirroring the static request-state pattern SpanManager /
     * RichTelemetryContext already use. A framework hook publishes the inbound context here so an
     * outbound-HTTP middleware — which only holds the child Span, not the request context — can forward
     * the inbound tracestate/baggage downstream. Best-effort and always cleared in the hook's finally.
     */
    private static ?self $ambient = null;
    public static function setAmbient(?self $context): void { self::$ambient = $context; }
    public static function ambient(): ?self { return self::$ambient; }

    /**
     * Re-renders the carried tracestate as a W3C tracestate header value, order-preserving. Returns
     * null when no tracestate is present so callers can skip emitting an empty header. The chronos
     * collector forwards inbound tracestate verbatim rather than mutating other vendors' state.
     */
    public function tracestateHeader(): ?string
    {
        if ($this->tracestate === null || $this->tracestate === []) {
            return null;
        }
        $members = [];
        foreach ($this->tracestate as $key => $value) {
            $members[] = $key.'='.$value;
        }

        return implode(',', $members);
    }

    /**
     * Re-renders the carried baggage as a W3C baggage header value, order-preserving, percent-encoding
     * each value. Properties (the optional ';'-separated metadata) are not re-emitted — carrying the
     * key/value pairs is sufficient for cross-service context and keeps the output unambiguous.
     */
    public function baggageHeader(): ?string
    {
        if ($this->baggage === null || $this->baggage === []) {
            return null;
        }
        $members = [];
        foreach ($this->baggage as $key => $value) {
            $members[] = $key.'='.rawurlencode($value);
        }

        return implode(',', $members);
    }

    /**
     * Parses a W3C tracestate header: a comma-separated list of key=value members, order-preserving,
     * de-duplicated (first occurrence wins), capped at 32 members. Malformed members are skipped, not
     * fatal. Never throws — a broken tracestate simply degrades to "no tracestate present".
     *
     * @return array<string,string>|null
     */
    private static function parseTracestate(?string $header): ?array
    {
        if (!is_string($header)) {
            return null;
        }
        $header = trim($header);
        if ($header === '') {
            return null;
        }
        $out = [];
        foreach (explode(',', $header) as $member) {
            if (count($out) >= self::MAX_TRACESTATE_ENTRIES) {
                break;
            }
            $member = trim($member);
            if ($member === '') {
                continue;
            }
            $eq = strpos($member, '=');
            if ($eq === false || $eq === 0) {
                continue;
            }
            $key = substr($member, 0, $eq);
            $value = substr($member, $eq + 1);
            if (!self::validTracestateKey($key) || !self::validTracestateValue($value)) {
                continue;
            }
            if (array_key_exists($key, $out)) {
                continue;
            }
            $out[$key] = $value;
        }

        return $out === [] ? null : $out;
    }

    private static function validTracestateKey(string $key): bool
    {
        // simple-key or multi-tenant tenant@system, per the tracestate ABNF.
        return preg_match('/^[a-z][a-z0-9_\-*\/]{0,255}$/D', $key) === 1
            || preg_match('/^[a-z0-9][a-z0-9_\-*\/]{0,240}@[a-z][a-z0-9_\-*\/]{0,13}$/D', $key) === 1;
    }

    private static function validTracestateValue(string $value): bool
    {
        $len = strlen($value);
        if ($len < 1 || $len > 256) {
            return false;
        }
        // Printable ASCII 0x20-0x7E excluding ',' and '=', and not ending in a blank.
        if (preg_match('/^[\x20-\x7e]+$/D', $value) !== 1) {
            return false;
        }
        if (str_contains($value, ',') || str_contains($value, '=')) {
            return false;
        }

        return substr($value, -1) !== ' ' && substr($value, -1) !== "\t";
    }

    /**
     * Parses a W3C baggage header: a comma-separated list of key=value members, each optionally
     * followed by ';'-separated properties (stripped here). Values are percent-decoded. Order is
     * preserved, keys de-duplicated (first wins), capped at 64 members and a bounded total size.
     * Never throws — a broken baggage header degrades to "no baggage present".
     *
     * @return array<string,string>|null
     */
    private static function parseBaggage(?string $header): ?array
    {
        if (!is_string($header)) {
            return null;
        }
        $header = trim($header);
        if ($header === '' || strlen($header) > self::MAX_BAGGAGE_HEADER_BYTES) {
            return null;
        }
        $out = [];
        foreach (explode(',', $header) as $member) {
            if (count($out) >= self::MAX_BAGGAGE_ENTRIES) {
                break;
            }
            $member = trim($member);
            if ($member === '') {
                continue;
            }
            // Drop the optional properties that follow the first ';'.
            $semi = strpos($member, ';');
            if ($semi !== false) {
                $member = substr($member, 0, $semi);
            }
            $member = trim($member);
            $eq = strpos($member, '=');
            if ($eq === false || $eq === 0) {
                continue;
            }
            $key = trim(substr($member, 0, $eq));
            $rawValue = trim(substr($member, $eq + 1));
            if ($key === '' || $rawValue === '' || !self::validBaggageKey($key)) {
                continue;
            }
            if (strlen($key) > self::MAX_BAGGAGE_KEY_BYTES) {
                continue;
            }
            $value = rawurldecode($rawValue);
            if (strlen($value) > self::MAX_BAGGAGE_VALUE_BYTES) {
                continue;
            }
            if (array_key_exists($key, $out)) {
                continue;
            }
            $out[$key] = $value;
        }

        return $out === [] ? null : $out;
    }

    private static function validBaggageKey(string $key): bool
    {
        // RFC 7230 token.
        return preg_match('/^[!#$%&\'*+\-.^_`|~0-9A-Za-z]+$/D', $key) === 1;
    }
}
