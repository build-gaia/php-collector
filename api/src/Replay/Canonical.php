<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * The Chronos Replay Protocol's canonical forms (protocol §5): the text form of a payload
 * value, the recorder's 4096-byte cap, selector normalisation, and the event digest.
 *
 * These four operations are the only places the protocol ever compares two values, so they
 * are the only places a language difference can manufacture a false divergence. Everything
 * here is therefore expressed in BYTES and avoids mbstring, iconv, locale and the
 * serialize_precision ini entirely: a runtime that compared "characters" would disagree
 * with a Go or Node runtime the moment a payload carried anything outside ASCII, and PHP
 * images built without mbstring are common enough that depending on it would make
 * conformance a property of the container rather than of the implementation.
 */
final class Canonical
{
    /**
     * The recorder's per-value cap (the native collector's MAX_PAYLOAD_VALUE_LENGTH). The
     * replay side has to apply it too — comparing an uncapped 4700-byte statement against
     * the recorder's capped 4096-byte copy misses every long statement and then aborts,
     * which is the failure the `lookup-selector-truncation` conformance case exists to catch.
     */
    public const VALUE_LIMIT = 4096;

    /** ASCII unit separator: between a payload key and its value in the digest input. */
    private const UNIT_SEPARATOR = "\x1F";

    /** ASCII record separator: after each key/value pair in the digest input. */
    private const RECORD_SEPARATOR = "\x1E";

    /** The whitespace run that selector normalisation collapses (protocol §6.2). */
    private const WHITESPACE = '/[\x09\x0A\x0B\x0C\x0D\x20]+/';

    /**
     * Canonical text form of a payload value (protocol §5.1), capped.
     *
     * The result is used for comparing, selecting and digesting only. It is never what the
     * replayed code receives: the value handed back keeps its original JSON type, because
     * canonicalisation is a comparison device and not a transformation of the answer.
     */
    public static function text(mixed $value): string
    {
        return self::cap(self::encode($value));
    }

    /**
     * Truncate to VALUE_LIMIT bytes, dropping a trailing partial UTF-8 sequence rather than
     * splitting it.
     *
     * The recorders truncate by byte-slicing without a boundary check, so a recording can
     * legitimately arrive with a broken sequence at the end of a long value. That is a
     * recorder defect, not a protocol variance: a conformant runtime drops the fragment and
     * carries on, because refusing the recording would lose a whole replay over one
     * unreadable trailing character.
     */
    public static function cap(string $value): string
    {
        if (strlen($value) <= self::VALUE_LIMIT) {
            return $value;
        }
        $capped = substr($value, 0, self::VALUE_LIMIT);
        $length = strlen($capped);
        // A UTF-8 sequence is at most four bytes, so the lead byte of a trailing sequence is
        // at most three continuation bytes back. Anything further back is malformed input we
        // leave exactly as the recorder wrote it.
        for ($offset = $length - 1; $offset >= 0 && $offset >= $length - 4; --$offset) {
            $byte = ord($capped[$offset]);
            if (($byte & 0xC0) === 0x80) {
                continue;
            }
            $width = match (true) {
                $byte < 0x80 => 1,
                ($byte & 0xE0) === 0xC0 => 2,
                ($byte & 0xF0) === 0xE0 => 3,
                ($byte & 0xF8) === 0xF0 => 4,
                default => 1,
            };

            return $offset + $width <= $length ? $capped : substr($capped, 0, $offset);
        }

        return $capped;
    }

    /**
     * Normalise a selector (protocol §6.2): cap, collapse every run of ASCII whitespace to
     * one space, strip the edges.
     *
     * The cap comes FIRST and that ordering is load-bearing. The recording side was capped
     * by the recorder on the raw value before any normalisation happened, so a replay that
     * collapsed first would keep more of a long statement than the recording holds and would
     * never match it. Collapsing only ever shortens, so no second cap is needed after it.
     */
    public static function selector(string $value): string
    {
        $collapsed = preg_replace(self::WHITESPACE, ' ', self::cap($value));

        return trim($collapsed ?? '', ' ');
    }

    /**
     * The protocol's own reproducible identity for an event (protocol §5.2).
     *
     * `payloadDigest` cannot serve this purpose: the recorder computes it over the payload's
     * recorded key order and its UNCAPPED values, then caps the values and serialises them
     * into a sorted map, so it is not recomputable from a materialised recording. The unit
     * and record separators are unprintable on purpose — a payload value containing `=` or a
     * newline (SQL and HTTP bodies routinely do) must not be able to forge another event's
     * digest.
     *
     * @param array<string, mixed> $payload
     */
    public static function eventDigest(string $kind, array $payload): string
    {
        $keys = array_map('strval', array_keys($payload));
        usort($keys, 'strcmp');
        $input = $kind."\n";
        foreach ($keys as $key) {
            $input .= $key.self::UNIT_SEPARATOR.self::text($payload[$key]).self::RECORD_SEPARATOR;
        }

        return 'sha256:'.hash('sha256', $input);
    }

    /** Canonical text of a value before the cap is applied. */
    private static function encode(mixed $value): string
    {
        if (is_string($value)) {
            return $value;
        }
        if (is_bool($value)) {
            return $value ? 'true' : 'false';
        }
        if ($value === null) {
            return 'null';
        }
        if (is_int($value)) {
            return (string) $value;
        }
        if (is_float($value)) {
            return self::float($value);
        }
        if (is_array($value) || $value instanceof \stdClass) {
            return self::json($value);
        }

        // Nothing else can come out of json_decode. An object a caller's expectation passed
        // in is stringified rather than refused: an expectation is diagnostic, and losing the
        // comparison would be worse than comparing a stringification.
        return is_object($value) && method_exists($value, '__toString') ? (string) $value : '';
    }

    /**
     * Compact JSON with object keys in UTF-8 byte order — hand-rolled because json_encode
     * cannot sort keys, and because its slash and unicode escaping is a per-call flag that a
     * Go or Node runtime would not reproduce. Slashes stay unescaped and non-ASCII stays raw
     * UTF-8, which is the form every other language's compact encoder produces by default.
     */
    private static function json(mixed $value): string
    {
        if ($value instanceof \stdClass) {
            $value = (array) $value;
        }
        if (!is_array($value)) {
            // Inside JSON a string is quoted and escaped; everywhere else the canonical text of
            // a string is the string itself. Delegating this case to encode() would emit a bare
            // `2` where `"2"` belongs and quietly equate a numeric string with a number.
            return is_string($value) ? self::string($value) : self::encode($value);
        }
        if (array_is_list($value)) {
            return '['.implode(',', array_map([self::class, 'json'], $value)).']';
        }
        $keys = array_map('strval', array_keys($value));
        usort($keys, 'strcmp');
        $pairs = [];
        foreach ($keys as $key) {
            $pairs[] = self::string($key).':'.self::json($value[$key]);
        }

        return '{'.implode(',', $pairs).'}';
    }

    private static function string(string $value): string
    {
        $encoded = json_encode(
            $value,
            JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE | JSON_INVALID_UTF8_SUBSTITUTE,
        );

        return $encoded === false ? '""' : $encoded;
    }

    /**
     * Shortest decimal form that round-trips back to the same double.
     *
     * json_encode() would be shorter to write but its output depends on the
     * serialize_precision ini, which differs between application images — and a digest that
     * depends on a php.ini is not a digest. The search below is ini-independent, so two PHP
     * builds agree with each other and with a Go or Rust `strconv`-style shortest form.
     */
    private static function float(float $value): string
    {
        if (!is_finite($value)) {
            return $value !== $value ? 'nan' : ($value > 0 ? 'inf' : '-inf');
        }
        for ($digits = 1; $digits <= 17; ++$digits) {
            $candidate = sprintf('%.'.$digits.'G', $value);
            if ((float) $candidate === $value) {
                return str_replace('E', 'e', $candidate);
            }
        }

        return str_replace('E', 'e', sprintf('%.17G', $value));
    }
}
