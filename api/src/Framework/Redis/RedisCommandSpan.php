<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Redis;

use Chronos\Collector\Service\SpanManager;

/**
 * Generic, Predis-free wrapper for turning a single Redis command dispatch into a
 * "redis <COMMAND>" child span. This package does not require predis/predis (deepwell wires
 * its own client in lib/helper/SharedCacheHelper.php via sfRedis, which is out of scope for
 * this repository), so this class only needs the command name, the raw argument list, and a
 * callable that performs the real dispatch; it times that callable directly rather than a
 * pre-execution-only hook, so duration reflects the real round trip.
 *
 * Verified against predis/predis: every command, including magic __call(), routes through the
 * single dispatch point Client::executeCommand(CommandInterface $command); CommandInterface::
 * getId() returns the uppercase command name and getArguments() the raw argument list. The
 * safest place to wire this is deepwell's own Redis client construction, by extending
 * \Predis\Client and overriding that one method:
 *
 * ```php
 * final class ChronosPredisClient extends \Predis\Client
 * {
 *     public function executeCommand(\Predis\Command\CommandInterface $command)
 *     {
 *         return RedisCommandSpan::around(
 *             $command->getId(),
 *             $command->getArguments(),
 *             fn () => parent::executeCommand($command),
 *         );
 *     }
 * }
 * ```
 *
 * That wiring change belongs in deepwell, not here; this class only provides the
 * production-safe, bounded span shape as a reusable building block.
 */
final class RedisCommandSpan
{
    /**
     * @template T
     * @param list<mixed> $arguments
     * @param callable(): T $execute
     * @return T
     */
    public static function around(string $command, array $arguments, callable $execute): mixed
    {
        $verb = strtoupper(trim($command));
        $span = SpanManager::open('redis '.($verb !== '' ? $verb : 'COMMAND'));
        if (!$span->isVoid()) {
            $span->add('db.system', 'redis');
            $span->add('db.operation', $verb !== '' ? $verb : 'COMMAND');
            if (
                isset($arguments[0])
                && is_scalar($arguments[0])
            ) {
                $span->add('db.redis.key', (string) $arguments[0]);
            }
        }

        try {
            return $execute();
        } finally {
            $span->finish();
        }
    }
}
