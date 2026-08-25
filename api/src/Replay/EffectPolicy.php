<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * Which effect mode governs a given (channel, intent) pair, resolved from the sandbox's
 * environment (protocol §7.2).
 *
 * The three named variables the scheduler already injects are the SPECIFIC cases and win.
 * Everything else resolves through the generic CHRONOS_REPLAY_EFFECT_<CHANNEL>_READ/_WRITE
 * pair, and that pair is the extension point the whole design turns on: a Node runtime that
 * records a channel PHP has never heard of gets a policy surface for it with no protocol
 * change and no scheduler change.
 */
final class EffectPolicy
{
    public const BLOCKED = 'blocked';
    public const REPLAYED = 'replayed';
    public const SIMULATED = 'simulated';
    public const PASSTHROUGH = 'passthrough';

    private const PREFIX = 'CHRONOS_REPLAY_EFFECT_';

    private const MODES = [self::BLOCKED, self::REPLAYED, self::SIMULATED, self::PASSTHROUGH];

    /** @param array<string, string> $environment */
    private function __construct(private readonly array $environment)
    {
    }

    /**
     * Validate every effect variable in the environment up front.
     *
     * Eagerly, not on first use: an invalid mode means the sandbox and the runtime disagree
     * about the contract, and the safe response to that is to run nothing at all rather than
     * to discover it half-way through a workload that has already had side effects. A missing
     * variable is fine and takes its default; a variable whose value is not one of the four
     * modes is never guessed at (protocol §7.2).
     *
     * @param array<string, string> $environment
     *
     * @throws PreconditionFailed
     */
    public static function fromEnvironment(array $environment): self
    {
        foreach ($environment as $name => $value) {
            if (!str_starts_with((string) $name, self::PREFIX)) {
                continue;
            }
            // An empty value counts as absent and takes the default. That is not a guess at a
            // mode — empty is not confusable with one — it is how POSIX tooling spells "unset",
            // and refusing to run a plan over a shell quirk would be its own failure mode.
            if ($value !== '' && !in_array($value, self::MODES, true)) {
                throw new PreconditionFailed(
                    Divergence::EFFECT_POLICY_INVALID,
                    sprintf('%s=%s is not one of %s', $name, var_export($value, true), implode('|', self::MODES)),
                );
            }
        }

        return new self($environment);
    }

    /**
     * The mode governing a lookup.
     *
     * The defaults are asymmetric on purpose. A READ defaults to `replayed` because answering
     * reads from the recording is what replay is, and a read that cannot be answered aborts
     * rather than reaching a live dependency — so the permissive-looking default is still
     * fail-closed with respect to the sandbox boundary. A WRITE defaults to `blocked`, which
     * matches the sandbox's own normalisation, because a write has an effect on the world and
     * the world is not part of the recording.
     */
    public function modeFor(string $channel, string $intent): string
    {
        $write = $intent === Vocabulary::INTENT_WRITE;
        if ($channel === Vocabulary::CHANNEL_DATABASE && $write) {
            return $this->mode('CHRONOS_REPLAY_EFFECT_DATABASE_WRITE', self::BLOCKED);
        }
        if ($channel === Vocabulary::CHANNEL_HTTP) {
            // Both intents, so an HTTP read is governed by the network variable and never by
            // CHRONOS_REPLAY_EFFECT_HTTP_READ. Reaching outside the sandbox is one decision,
            // not two.
            return $this->mode('CHRONOS_REPLAY_EFFECT_NETWORK_CALL', self::BLOCKED);
        }
        if (Vocabulary::isQueueChannel($channel) && $write) {
            return $this->mode('CHRONOS_REPLAY_EFFECT_QUEUE_PUBLISH', self::BLOCKED);
        }
        $token = Vocabulary::environmentToken($channel);

        return $write
            ? $this->mode(self::PREFIX.$token.'_WRITE', self::BLOCKED)
            : $this->mode(self::PREFIX.$token.'_READ', self::REPLAYED);
    }

    private function mode(string $name, string $fallback): string
    {
        $value = $this->environment[$name] ?? '';

        return $value === '' ? $fallback : $value;
    }
}
