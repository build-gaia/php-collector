<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * The effect policy refused this operation (protocol §7.3).
 *
 * PHP's natural error for a refused operation is an exception, so that is what the replayed
 * code gets. Application code is entitled to catch it — a retry path or a graceful degradation
 * is a legitimate thing to exercise under replay — but the runtime never catches it on the
 * application's behalf, because swallowing it to keep the replay going would turn `blocked`
 * into a silent success.
 */
final class ReplayBlocked extends \RuntimeException
{
    public function __construct(public readonly Answer $answer)
    {
        parent::__construct(sprintf(
            'chronos replay: %s %s of "%s" is blocked by effect policy',
            $answer->channel,
            $answer->intent,
            $answer->selector,
        ));
    }
}
