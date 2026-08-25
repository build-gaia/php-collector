<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * A fatal condition detected BEFORE the first lookup: the recording could not be read or
 * trusted, its protocol major is unreadable, or an effect variable carried a mode outside the
 * contract (protocol §10).
 *
 * It is a distinct exception rather than a divergence returned inline because a precondition
 * failure means no verdict was reached at all — the plan, the mount or the policy was wrong,
 * and nothing whatsoever was learned about the application code. ReplaySession catches it,
 * writes a report with outcome `precondition_failed`, and refuses to run the workload; it
 * never escapes into application code.
 */
final class PreconditionFailed extends \RuntimeException
{
    public function __construct(
        public readonly string $type,
        string $message,
    ) {
        parent::__construct($message);
    }
}
