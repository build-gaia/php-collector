<?php

declare(strict_types=1);

/**
 * Replay-mode bootstrap, intended as PHP's `auto_prepend_file` in a replay image.
 *
 * auto_prepend_file rather than a framework hook or a Composer autoload entry, because the
 * recording has to be discovered and the report armed BEFORE any application code runs: a fatal
 * in the framework's own bootstrap would otherwise produce a container that exits non-zero with
 * no report, which reads to an operator as a broken plan rather than as the broken code it is.
 *
 * The module's classes are required by path rather than autoloaded, because auto_prepend_file
 * runs before the application has loaded Composer's autoloader — and requiring the application's
 * autoloader from here would make the replay shim depend on where the app keeps its vendor tree.
 *
 * Harmless outside replay mode: with no CHRONOS_REPLAY_RECORDING in the environment,
 * ReplayRuntime::boot() returns null, reads nothing and writes nothing, so one image serves both
 * the recording run and the replay of it.
 */

use Chronos\Collector\Replay\ReplayRuntime;

foreach ([
    'Canonical.php',
    'Vocabulary.php',
    'RecordedEvent.php',
    'PreconditionFailed.php',
    'Divergence.php',
    'Lookup.php',
    'Answer.php',
    'Recording.php',
    'EffectPolicy.php',
    'Report.php',
    'ReplaySession.php',
    'ReplayRuntime.php',
    'ReplayBlocked.php',
    'ReplayAborted.php',
    'Effect.php',
] as $unit) {
    require_once __DIR__.'/'.$unit;
}

if (!function_exists('chronos_replay_effect_delegate')) {
    /**
     * Native scalar hooks call this by name. Returns an Effect payload array or null to
     * fall through to the real builtin (non-replay / miss handling stays in Effect).
     *
     * @return array<string, mixed>|null
     */
    function chronos_replay_effect_delegate(string $kind, string $selector): ?array
    {
        return match ($kind) {
            'time' => \Chronos\Collector\Replay\Effect::time($selector),
            'random' => \Chronos\Collector\Replay\Effect::random($selector),
            'env' => \Chronos\Collector\Replay\Effect::environment($selector),
            default => null,
        };
    }
}

ReplayRuntime::boot();
