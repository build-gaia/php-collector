<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * One observed difference between the replay and the recording, or one refusal to proceed
 * (protocol §8).
 *
 * Divergence is the DELIVERABLE of a replay, not its error channel: a run that finds twelve
 * divergences did its job. The v1 type set below is closed — within a major version a runtime
 * must not invent a type — because the report is meant to be diffed in CI and a consumer that
 * met an unknown type would have to choose between ignoring it and failing on it.
 */
final class Divergence
{
    public const UNRECORDED_EFFECT = 'unrecorded_effect';
    public const ORDINAL_EXHAUSTED = 'ordinal_exhausted';
    public const EFFECT_BLOCKED = 'effect_blocked';
    public const SIMULATED_SUBSTITUTION = 'simulated_substitution';
    public const PASSTHROUGH_MISMATCH = 'passthrough_mismatch';
    public const PASSTHROUGH_PERFORMED = 'passthrough_performed';
    public const UNCONSUMED_EVENT = 'unconsumed_event';
    public const VALUE_MISMATCH = 'value_mismatch';
    public const RECORDING_TRUNCATED = 'recording_truncated';
    public const PROTOCOL_UNSUPPORTED = 'protocol_unsupported';
    public const RECORDING_UNAVAILABLE = 'recording_unavailable';
    public const EFFECT_POLICY_INVALID = 'effect_policy_invalid';
    public const EFFECT_UNAVAILABLE = 'effect_unavailable';

    public const FATAL = 'fatal';
    public const DIVERGENT = 'divergent';
    public const INFORMATIONAL = 'informational';

    /**
     * Severity is fixed per type, with one exception: `unconsumed_event` depends on
     * CHRONOS_REPLAY_STRICT, because "this code no longer performs a recorded effect" is a
     * finding under `full` and expected background noise under `answers`.
     */
    private const SEVERITIES = [
        self::UNRECORDED_EFFECT => self::FATAL,
        self::ORDINAL_EXHAUSTED => self::FATAL,
        self::EFFECT_BLOCKED => self::DIVERGENT,
        self::SIMULATED_SUBSTITUTION => self::DIVERGENT,
        self::PASSTHROUGH_MISMATCH => self::DIVERGENT,
        self::PASSTHROUGH_PERFORMED => self::INFORMATIONAL,
        self::UNCONSUMED_EVENT => self::DIVERGENT,
        self::VALUE_MISMATCH => self::DIVERGENT,
        self::RECORDING_TRUNCATED => self::INFORMATIONAL,
        self::PROTOCOL_UNSUPPORTED => self::FATAL,
        self::RECORDING_UNAVAILABLE => self::FATAL,
        self::EFFECT_POLICY_INVALID => self::FATAL,
        self::EFFECT_UNAVAILABLE => self::FATAL,
    ];

    /**
     * @param array<string, mixed>|null $expected describes the recording
     * @param array<string, mixed>|null $observed describes the replay
     */
    public function __construct(
        public readonly int $step,
        public readonly string $type,
        public readonly string $severity,
        public readonly ?string $channel = null,
        public readonly ?string $selector = null,
        public readonly ?string $intent = null,
        public readonly ?int $ordinal = null,
        public readonly ?string $mode = null,
        public readonly ?int $sequence = null,
        public readonly ?array $expected = null,
        public readonly ?array $observed = null,
        public readonly ?string $site = null,
        public readonly string $message = '',
    ) {
    }

    public static function severityOf(string $type, string $strict = ReplaySession::STRICT_FULL): string
    {
        if ($type === self::UNCONSUMED_EVENT && $strict === ReplaySession::STRICT_ANSWERS) {
            return self::INFORMATIONAL;
        }

        return self::SEVERITIES[$type] ?? self::DIVERGENT;
    }

    /** @return array<string, mixed> */
    public function toArray(): array
    {
        return [
            'step' => $this->step,
            'type' => $this->type,
            'severity' => $this->severity,
            'channel' => $this->channel,
            'selector' => $this->selector,
            'intent' => $this->intent,
            'ordinal' => $this->ordinal,
            'mode' => $this->mode,
            'sequence' => $this->sequence,
            'expected' => $this->expected,
            'observed' => $this->observed,
            // Diagnostics only, never part of selection: a replay exists to run EDITED code,
            // and editing moves call sites (protocol §6.6).
            'site' => $this->site,
            'message' => $this->message,
        ];
    }
}
