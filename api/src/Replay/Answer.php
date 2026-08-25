<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * The resolution of one lookup: what the replayed code receives and how it was arrived at.
 *
 * `payload` keeps the recording's original JSON types — a recorded number is handed back as a
 * number — because canonicalisation exists for comparison and is not a transformation of the
 * answer. `fatal` is the flag the interception layer acts on: the protocol engine records the
 * finding and hands it back, and the layer above turns it into the process's death rather
 * than a value.
 */
final class Answer
{
    public const HIT = 'hit';
    public const MISS = 'miss';
    public const BLOCKED = 'blocked';
    public const SIMULATED = 'simulated';
    public const PASSTHROUGH = 'passthrough';

    /**
     * @param array<string, mixed>|null $payload
     */
    public function __construct(
        public readonly int $step,
        public readonly string $outcome,
        public readonly ?array $payload,
        public readonly ?int $sequence,
        public readonly ?int $answerSequence,
        public readonly ?string $eventDigest,
        public readonly ?string $payloadDigest,
        public readonly ?string $redaction,
        public readonly string $channel,
        public readonly string $selector,
        public readonly string $intent,
        public readonly int $ordinal,
        public readonly string $mode,
        public readonly bool $fatal,
        public readonly ?string $divergence = null,
    ) {
    }

    /**
     * The per-lookup record the report carries.
     *
     * The protocol's own report schema names this array `consumptions` and does not carry the
     * outcome or the value; both are added here, because a conformance runner and a human
     * reading a diff both need to see WHAT the replay was handed, not only which event it came
     * from. Consumers ignore fields they do not know, so adding them costs nothing.
     *
     * @return array<string, mixed>
     */
    public function toArray(): array
    {
        return [
            'step' => $this->step,
            'outcome' => $this->outcome,
            'channel' => $this->channel,
            'selector' => $this->selector,
            'intent' => $this->intent,
            'ordinal' => $this->ordinal,
            'mode' => $this->mode,
            'sequence' => $this->sequence,
            'answerSequence' => $this->answerSequence,
            'value' => $this->payload,
            'eventDigest' => $this->eventDigest,
            'payloadDigest' => $this->payloadDigest,
            'redaction' => $this->redaction,
        ];
    }
}
