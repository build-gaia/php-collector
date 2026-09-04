<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Predis;

use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Predis\Command\CommandInterface;
use Predis\Pipeline\Pipeline;
use Throwable;

/**
 * A Predis pipeline that reports what it flushed.
 *
 * `ChronosPredisClient` spans commands by wrapping `__call`, which is how every
 * ordinary Predis command is dispatched — but `pipeline()` is a REAL method on
 * `Predis\Client`, so it never reaches that wrapper. Everything sent through a
 * pipeline was therefore invisible, and a pipeline is not a rare corner: it is
 * what code reaches for precisely when it has a lot of Redis work to do. A cache
 * whose reads were traced and whose writes were not is worse than one that traced
 * neither, because the trace looks complete.
 *
 * One span per flush, not per queued command. The round trip is the thing that
 * costs — that is the entire point of pipelining — and thirty spans all claiming
 * the duration of the single flush they were batched into would be thirty wrong
 * numbers instead of one right one. The commands are named on the span, so what
 * went in it is still legible.
 *
 * A real `Pipeline` subclass rather than a decorator, so callers that type-hint
 * or instanceof-check the pipeline they were handed keep working.
 */
final class ChronosPipeline extends Pipeline
{
    /** How many commands were queued before this flush. */
    private int $queued = 0;

    /**
     * Distinct command names, bounded. A pipeline that writes the same key ten
     * thousand times should cost one attribute, not ten thousand entries.
     *
     * @var array<string, true>
     */
    private array $commandNames = [];

    private const MAX_COMMAND_NAMES = 16;

    /**
     * Both `__call` and `executeCommand` funnel through here, so counting in one
     * place catches every way a command can enter the buffer.
     */
    protected function recordCommand(CommandInterface $command)
    {
        try {
            ++$this->queued;
            if (count($this->commandNames) < self::MAX_COMMAND_NAMES) {
                $id = strtoupper((string) $command->getId());
                if ($id !== '') {
                    $this->commandNames[$id] = true;
                }
            }
        } catch (Throwable) {
            // Counting must never stop the command being queued.
        }

        return parent::recordCommand($command);
    }

    /**
     * @param  mixed $callable
     * @return array
     */
    public function execute($callable = null)
    {
        $span = null;
        try {
            $span = SpanManager::open('redis PIPELINE');
        } catch (Throwable) {
            $span = null;
        }

        try {
            return parent::execute($callable);
        } finally {
            // Stamped AFTER the call: with a callable, the commands are queued
            // inside parent::execute, so before it the counters are still zero.
            if ($span instanceof Span) {
                try {
                    if (!$span->isVoid()) {
                        $span->add('cache.system', 'redis');
                        $span->add('cache.store', 'redis');
                        $span->add('db.operation', 'PIPELINE');
                        $span->add('db.redis.commands', (string) $this->queued);
                        if ($this->commandNames !== []) {
                            $names = array_keys($this->commandNames);
                            sort($names);
                            $span->add('db.redis.command.names', implode(',', $names));
                        }
                        if (count($this->commandNames) >= self::MAX_COMMAND_NAMES) {
                            $span->add('db.redis.command.names.truncated', 'true');
                        }
                    }
                    $span->finish();
                } catch (Throwable) {
                }
            }
        }
    }
}
