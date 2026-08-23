<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * One recorded non-deterministic interaction, with the derived facts the protocol selects on
 * (channel, selector, role, event digest) resolved once at load.
 *
 * Immutable, and deliberately so: the recording is shared and read-only, and a second
 * concurrent replay of the same recording must not be able to observe the first. Everything
 * that changes during a replay — cursors, consumption — is session state and lives in
 * ReplaySession, not here.
 */
final class RecordedEvent
{
    /**
     * @param array<string, mixed> $payload
     */
    public function __construct(
        public readonly int $sequence,
        public readonly string $timestamp,
        public readonly string $kind,
        public readonly array $payload,
        public readonly string $payloadDigest,
        public readonly string $redaction,
        public readonly string $channel,
        public readonly string $role,
        public readonly string $selector,
        public readonly string $eventDigest,
    ) {
    }

    /**
     * @param array<string, mixed> $raw
     */
    public static function fromArray(array $raw): self
    {
        $kind = is_string($raw['kind'] ?? null) ? $raw['kind'] : '';
        $payload = is_array($raw['payload'] ?? null) ? $raw['payload'] : [];
        $channel = Vocabulary::channelForKind($kind);

        return new self(
            sequence: is_numeric($raw['sequence'] ?? null) ? (int) $raw['sequence'] : 0,
            timestamp: is_string($raw['timestamp'] ?? null) ? $raw['timestamp'] : '',
            kind: $kind,
            payload: $payload,
            // Carried through untouched and never verified: it is provenance the recorder
            // computed over uncapped values in their recorded key order, so it cannot be
            // recomputed from a materialised recording (protocol §3.4).
            payloadDigest: is_string($raw['payloadDigest'] ?? null) ? $raw['payloadDigest'] : '',
            // Surfaced on every answer so the report's reader can tell a complete value from
            // a shortened one. An unknown value is kept verbatim rather than normalised: a
            // future recorder's redaction vocabulary must not be flattened into today's.
            redaction: is_string($raw['redaction'] ?? null) ? $raw['redaction'] : '',
            channel: $channel,
            role: Vocabulary::roleForKind($kind),
            selector: Vocabulary::selectorFor($channel, $payload),
            eventDigest: Canonical::eventDigest($kind, $payload),
        );
    }
}
