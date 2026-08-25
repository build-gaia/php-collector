<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * Writes the replay report (protocol §9).
 *
 * The report is a FILE and not a stream, for two reasons that are properties of the sandbox
 * rather than preferences: `/workspace` is the only writable bind mount and therefore the only
 * thing that survives the container, while `/tmp` is a bounded noexec tmpfs that does not; and
 * stdout is bounded by the executor's output limit and interleaved with application output, so
 * a long report would be silently clipped in the middle of the findings.
 */
final class Report
{
    public const SCHEMA = 'chronos.replay.report.v1';

    /** Where the sandbox's writable bind mount is, when the plan names nothing else. */
    public const DEFAULT_PATH = '/workspace/chronos-replay-report.json';

    /** The prefix of the single-line stderr fallback, so a log scraper can find it. */
    public const STDERR_PREFIX = 'chronos-replay-report: ';

    /**
     * Write the report, falling back to one JSON line on stderr when the path is unwritable.
     *
     * The fallback line may be clipped by the executor's output bound; the exit code will not
     * be, which is why the exit code is settled by the caller before this is ever called. A
     * replay that could not write its report still has to tell the scheduler what it found.
     *
     * @param array<string, mixed> $report
     */
    public static function write(array $report, string $path): bool
    {
        $encoded = json_encode(
            $report,
            JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE | JSON_PARTIAL_OUTPUT_ON_ERROR,
        );
        if ($encoded === false) {
            $encoded = '{"schema":"'.self::SCHEMA.'","outcome":"'.($report['outcome'] ?? '').'"}';
        }
        $directory = \dirname($path);
        if (!is_dir($directory)) {
            @mkdir($directory, 0o700, true);
        }
        if (@file_put_contents($path, $encoded."\n") !== false) {
            return true;
        }
        // Never silent: a missing report with a bare exit code leaves the operator guessing
        // whether the replay found nothing or never ran.
        @file_put_contents('php://stderr', self::STDERR_PREFIX.$encoded."\n");

        return false;
    }
}
