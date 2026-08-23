<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * One request from the replayed code for a recorded answer: `(channel, selector, intent)`,
 * plus two things that never affect selection.
 *
 * The selector arrives PRE-DERIVATION — the raw string the calling code has in hand — and is
 * normalised here, by the same code path that normalised the recording side. Running one
 * derivation over both sides is the whole reason re-formatted SQL still matches.
 */
final class Lookup
{
    public readonly string $selector;

    /**
     * @param array<string, mixed>|null $expectation a partial payload the caller believes the
     *                                               answer satisfies (protocol §6.5)
     * @param string|null               $site        call-site identity for the report only
     */
    public function __construct(
        public readonly string $channel,
        string $selector,
        public readonly string $intent = Vocabulary::INTENT_READ,
        public readonly ?array $expectation = null,
        public readonly ?string $site = null,
    ) {
        $this->selector = Canonical::selector($selector);
    }
}
