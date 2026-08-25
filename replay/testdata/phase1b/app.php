<?php

declare(strict_types=1);

/**
 * ADR 0021 scalar interception acceptance: application code calls PHP builtins.
 * Requires chronos.so with chronos_replay_delegate armed by ReplayRuntime bootstrap.
 */

$time = time();
$rand = mt_rand();
fwrite(STDOUT, "PHASE1B_APP_RAN time={$time} rand={$rand}\n");
