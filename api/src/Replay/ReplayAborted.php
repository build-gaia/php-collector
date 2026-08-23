<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * The replay hit something the recording cannot answer and has stopped (protocol §7.1, §10).
 *
 * In a real replay the process exits 92 and this is never seen: aborting is not an error the
 * application may handle, because continuing past an unanswerable call is exactly the silent
 * wrongness the protocol exists to prevent. It exists for the case where the terminator has
 * been replaced — a conformance runner, a test — so the abort is observable in-process instead
 * of taking the interpreter down with it.
 */
final class ReplayAborted extends \RuntimeException
{
    public function __construct(public readonly Answer $answer)
    {
        parent::__construct(sprintf(
            'chronos replay: aborting, %s (%s %s of "%s", ordinal %d)',
            (string) $answer->divergence,
            $answer->channel,
            $answer->intent,
            $answer->selector,
            $answer->ordinal,
        ));
    }
}
