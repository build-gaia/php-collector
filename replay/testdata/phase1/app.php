<?php

declare(strict_types=1);

/**
 * Minimal application entry for ADR 0021 Phase 1.
 *
 * Proves a real PHP entrypoint ran under the replay shim (auto_prepend_file), consumed one
 * recorded clock effect through Chronos\Collector\Replay\Effect, and printed a marker the
 * acceptance harness can assert. No framework, no live I/O — mercury packaging is a later
 * image concern; this fixture is the executable proof the scheduler/image seam must satisfy.
 */

use Chronos\Collector\Replay\Effect;

$answer = Effect::time('time');
if ($answer === null) {
    fwrite(STDERR, "PHASE1_NOT_REPLAY: Effect::time returned null (shim not armed)\n");
    exit(1);
}

$result = $answer['result'] ?? '';
fwrite(STDOUT, "PHASE1_APP_RAN result={$result}\n");
