<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Throwable;

/**
 * The payload-capture decision for a message, in one place both halves of a
 * stream call.
 *
 * A messaging body is the one attribute on a messaging span that is neither
 * bounded nor safe by construction: a queue name is an operator-chosen word, a
 * routing key is at worst an entity id, but a payload is whatever the producer
 * serialised — arbitrary length, arbitrary bytes, and quite possibly a
 * customer's address. So the encoding is the part worth pinning down in a test,
 * and it lives here rather than being written twice (once on the publish side
 * where it rides a span attribute, once on the consume side where it rides the
 * request-attribute bag) and drifting.
 *
 * ## Off by default, which is the opposite of the HTTP path
 *
 * `http_capture_bodies` defaults ON because an operator installing an APM agent
 * has already decided to look at their own request and response bodies. An
 * inter-service message is a different decision: the payload was written by one
 * team for another team's consumer, and capturing it copies that contract into
 * telemetry a third audience reads. `messaging_capture_bodies` therefore
 * defaults OFF, and with it off not one byte of payload is copied, encoded or
 * even measured beyond `strlen()` — the gate is the FIRST thing checked, before
 * the emptiness test and before the cap is resolved.
 *
 * ## No field-level masking, stated rather than implied
 *
 * There is no key-level redaction for a body on EITHER path in this SDK. The
 * native side's `http_capture.rs` masks map ENTRIES — headers and query
 * parameters, things with a key to match against — while `body_attributes()`
 * truncates a body and does nothing else. A message payload has no map of
 * fields the collector understands (it may be protobuf, which has no field
 * NAMES on the wire at all), so there is nothing to key-match on. The env gate
 * IS the control. Saying so here is deliberate: an operator who assumed
 * symmetry with the HTTP redaction would turn this on expecting a safety net
 * that does not exist.
 *
 * ## base64, not raw, not hex
 *
 * A protobuf `serializeToString()` is not valid UTF-8, and a captured body has
 * to survive two UTF-8-only crossings: ext-php-rs hands it to a Rust `String`,
 * and `serde_json` serialises it into the spool envelope. Worse, `Span::cap()`
 * truncates with byte-based `substr()`, so a raw binary body that happened to
 * pass both crossings could still be cut mid-character and invalidate an
 * otherwise-good UTF-8 payload downstream. Base64 makes every byte sequence
 * representable; hex would do the same at 2x the size rather than 1.33x, for
 * exactly the same information.
 *
 * Framework-free by design — no vendor types, no Laravel, no .so required to
 * declare it — which is what makes the standalone test suite able to prove the
 * encoding without a broker.
 */
final class MessagingBody
{
    public const BODY = 'messaging.message.body';
    public const ENCODING = 'messaging.message.body.encoding';
    public const TRUNCATED = 'messaging.message.body.truncated';

    /**
     * Set only when the collector has actually TAKEN a whole copy of the payload
     * for the span-body store — i.e. when `chronos_store_span_body` returned
     * true. It is a promise a reader acts on (it is what turns on "load the rest"
     * in the desktop), so it is never set optimistically: a `.stored` marker that
     * resolves to nothing is the single failure that store was built to prevent.
     */
    public const STORED = 'messaging.message.body.stored';

    /**
     * The body attributes for one message, or an empty array when there are
     * none to emit.
     *
     * Always a SUBSET of {body, encoding, truncated}, never a shape with empty
     * values in it, so a caller can `+=` the result unconditionally without
     * first asking whether capture is on.
     *
     * `$ceiling` is the caller's own hard limit, not a preference: a publish
     * body rides a span attribute capped at `Span::MAX_TEXT_LENGTH` (16384),
     * while a consume body rides the request-attribute bag capped at the native
     * side's `MAX_VALUE_BYTES` (8192). The configured setting is min()'d against
     * it so the truncation happens HERE, in PHP, where `.truncated` can be set
     * honestly — if the value were left to be cut by `Span::cap()` or by the
     * native cap, an oversized body would arrive looking COMPLETE, which is the
     * precise failure `NativeExtension::bodyCaptureCeiling`'s own docblock was
     * written about.
     *
     * @return array<string, string>
     */
    public static function encode(string $body, int $ceiling): array
    {
        try {
            // First, before anything touches the payload: with capture off the
            // body is never read, never copied and never measured here.
            if (!NativeExtension::messagingCapturing()) {
                return [];
            }
            if ($body === '') {
                return [];
            }
            $cap = min(NativeExtension::messagingBodyCeiling(), $ceiling);
            if ($cap <= 0) {
                return [];
            }

            return self::isText($body) ? self::text($body, $cap) : self::binary($body, $cap);
        } catch (Throwable) {
            // A body that cannot be encoded is a body that is not reported. It
            // must never be a publish or a consume that failed.
            return [];
        }
    }

    /**
     * The WHOLE payload for the span-body store, as `[payload, encoding]`, or
     * `['', '']` when there is nothing to store.
     *
     * The sibling of [`encode`] and deliberately not a widening of it: they
     * answer different questions against the same bytes. `encode` produces the
     * span's PREVIEW, cut to whatever the attribute it rides can hold (16 KiB on
     * a publish span, 8 KiB in the consume side's request-attribute bag). This
     * produces the copy that goes to the blob store, cut only to what the
     * operator allowed — `CHRONOS_PHP_MESSAGING_CAPTURE_MAX_BODY`, 64 KiB by
     * default and hard-clamped to 512 KiB. Same gate, same `isText()` test, same
     * cut-before-encode rule, so the preview and the stored copy can never
     * disagree about the same payload; one implementation of each is exactly why
     * they cannot.
     *
     * `$previewCeiling` is the bound the caller's preview was already cut to, and
     * nothing is stored unless the allowance genuinely exceeds it — mirroring
     * `http_capture`'s own rule (`if !overflowed || max_body_total_bytes <=
     * max_body_bytes`). A blob identical to the attribute beside it costs a NATS
     * message, a hypertable row and a round trip to say what the span already
     * said.
     *
     * The returned encoding is a TRANSFER encoding (`base64` or `''`), and it is
     * only ever handed to the native store. The authority a reader consults is
     * the span's [`ENCODING`] attribute, which [`encode`] set from the identical
     * test — one fact, not two that can drift.
     *
     * NOTE: the contract this was written against spells the signature
     * `whole(string $body)`. It cannot be implemented that way: the
     * "only when it exceeds the preview" rule needs the preview's bound, and the
     * preview bound is per call site (publish 16 KiB, consume 8 KiB), so it has
     * to be passed in exactly as `encode()` already takes it.
     *
     * @return array{0: string, 1: string}
     */
    public static function whole(string $body, int $previewCeiling): array
    {
        try {
            // The gate first, before the payload is read, copied or measured —
            // the same ordering [`encode`] documents.
            if (!NativeExtension::messagingCapturing()) {
                return ['', ''];
            }
            if ($body === '') {
                return ['', ''];
            }
            $budget = NativeExtension::messagingBodyCeiling();
            if ($budget <= 0 || $budget <= $previewCeiling) {
                return ['', ''];
            }
            $encoded = self::isText($body) ? self::text($body, $budget) : self::binary($body, $budget);
            $payload = $encoded[self::BODY] ?? '';
            if ($payload === '') {
                return ['', ''];
            }

            return [$payload, $encoded[self::ENCODING] ?? ''];
        } catch (Throwable) {
            // A payload that cannot be encoded is a payload that is not stored.
            // It must never be a publish or a consume that failed.
            return ['', ''];
        }
    }

    /**
     * Whether this payload can go out as text: valid UTF-8, with no C0 control
     * bytes other than tab, LF and CR.
     *
     * Two tests rather than one because either alone is wrong. Valid UTF-8 is
     * necessary but not sufficient — a protobuf message of small positive
     * varints is legal UTF-8 and is still binary, and shipping it as "text"
     * produces an attribute full of control characters that no UI can render.
     * The control-byte test alone would admit a Latin-1 payload that breaks the
     * Rust crossing. Tab/LF/CR are exempted because they appear in perfectly
     * ordinary pretty-printed JSON and XML.
     */
    private static function isText(string $body): bool
    {
        return \preg_match('//u', $body) === 1
            && \preg_match('/[\x00-\x08\x0B\x0C\x0E-\x1F]/', $body) !== 1;
    }

    /**
     * A text body, cut on a BYTE budget without splitting a character.
     *
     * The budget is bytes because every downstream limit is in bytes, but the
     * cut must land on a character boundary or the result is no longer valid
     * UTF-8 — which would defeat the whole point of having taken the text path.
     * `mb_strcut` does exactly that; where mbstring is absent (it is not a
     * dependency of this package, and a minimal container legitimately lacks
     * it) the same result is reached by cutting bytes and then dropping up to
     * three trailing bytes until the remainder validates, since a UTF-8
     * character is at most four bytes long.
     *
     * No `encoding` key on this path: text is the default, and stamping
     * `encoding=utf-8` onto every JSON body would be noise on every span in the
     * estate to say the unremarkable thing.
     *
     * @return array<string, string>
     */
    private static function text(string $body, int $cap): array
    {
        if (\function_exists('mb_strcut')) {
            $text = \mb_strcut($body, 0, $cap, 'UTF-8');
        } else {
            $text = \substr($body, 0, $cap);
            for ($drop = 0; $drop < 3 && $text !== '' && \preg_match('//u', $text) !== 1; ++$drop) {
                $text = \substr($text, 0, -1);
            }
        }
        $attributes = [self::BODY => $text];
        if (\strlen($text) < \strlen($body)) {
            $attributes[self::TRUNCATED] = 'true';
        }

        return $attributes;
    }

    /**
     * A binary body, base64-encoded into the budget rather than over it.
     *
     * The raw bytes are cut FIRST, to the largest whole-triplet length whose
     * encoding fits (`intdiv($cap, 4) * 3`), and only then encoded. Encoding
     * first and cutting after would both overshoot the caller's ceiling by 1.33x
     * — so the real cut would happen later, in native code that cannot set
     * `.truncated` — and could sever a 4-character base64 group, producing a
     * value that no longer decodes. Cutting first means the emitted attribute
     * lands at or under the cap and decodes cleanly to a genuine prefix of the
     * payload.
     *
     * @return array<string, string>
     */
    private static function binary(string $body, int $cap): array
    {
        $budget = \intdiv($cap, 4) * 3;
        if ($budget <= 0) {
            return [];
        }
        $raw = \substr($body, 0, $budget);
        $attributes = [
            self::BODY => \base64_encode($raw),
            self::ENCODING => 'base64',
        ];
        if (\strlen($raw) < \strlen($body)) {
            $attributes[self::TRUNCATED] = 'true';
        }

        return $attributes;
    }
}
