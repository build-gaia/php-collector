<?php

declare(strict_types=1);

/**
 * One conformance case, run as its own process.
 *
 * A subprocess rather than an in-process harness for two reasons the suite's runner contract
 * asks for directly. A variable absent from a case's env.json has to be absent from the
 * environment the implementation sees, and only a freshly-spawned process with an explicit
 * environment gives that — merging over the inherited environment would quietly satisfy cases
 * that exist to prove the opposite. And the protocol's exit codes are part of the contract, so
 * they are checked as real process exit codes rather than as a return value the harness could
 * have massaged.
 *
 * Nothing here is a test double. The child boots the same ReplayRuntime an application image
 * boots, through the same bootstrap, and lets the same shutdown handler settle the exit code.
 *
 * Usage: php replay-case.php <case-directory>
 */

use Chronos\Collector\Replay\Effect;
use Chronos\Collector\Replay\Lookup;
use Chronos\Collector\Replay\ReplayBlocked;
use Chronos\Collector\Replay\ReplayRuntime;

require_once __DIR__.'/../src/Replay/bootstrap.php';

$caseDirectory = $argv[1] ?? '';
if ($caseDirectory === '') {
    fwrite(STDERR, "usage: replay-case.php <case-directory>\n");
    exit(64);
}

$session = ReplayRuntime::session();
if ($session === null) {
    fwrite(STDERR, "CHRONOS_REPLAY_RECORDING was not set: this process is not a replay\n");
    exit(64);
}

$lookups = json_decode((string) file_get_contents($caseDirectory.'/lookups.json'), true);
foreach ($lookups['lookups'] ?? [] as $entry) {
    if (!$session->isRunning()) {
        // Discovery refused the recording or the policy. The report is already written and the
        // shutdown handler owns the exit code; issuing lookups now would invent findings.
        break;
    }
    $live = null;
    if (array_key_exists('live', $entry)) {
        $live = static fn (): array => $entry['live'];
    } elseif (array_key_exists('liveError', $entry)) {
        // The live dependency is down. Supplied as fixture data so the case runs with the
        // sandbox network off and without a real dependency to take away.
        $live = static function () use ($entry): array {
            throw new \RuntimeException((string) $entry['liveError']);
        };
    }
    try {
        Effect::resolve(
            new Lookup(
                (string) $entry['channel'],
                (string) $entry['selector'],
                (string) $entry['intent'],
                $entry['value'] ?? null,
            ),
            $live,
        );
    } catch (ReplayBlocked) {
        // What application code sees when policy refuses an effect. Caught and carried on with
        // deliberately: the next lookup in the case is the assertion that a block does not end
        // the replay. An unanswerable lookup never reaches here — it exits the process.
    }
}

// No explicit finish: the shutdown handler ReplayRuntime::boot() registered writes the report
// and forces the exit code, exactly as it does in an application image.
