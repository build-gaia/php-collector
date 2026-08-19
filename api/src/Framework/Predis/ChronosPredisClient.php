<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Predis;

use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Throwable;

/**
 * Drop-in replacement for Predis\Client that turns each Redis command into a child span under the
 * current request span. Predis dispatches every command (get/set/hmget/…) through __call, so a
 * subclass that wraps __call captures them all without decorating individual call sites. Real
 * Client methods (pipeline/transaction/getConnection/…) are untouched and behave exactly as before.
 *
 * Frameworks with no global Redis hook (e.g. Symfony 1 apps using Predis directly) can opt in by
 * constructing this class instead of Predis\Client — the constructor signature is inherited, so it
 * is a straight swap. Every telemetry step is fully defensive: it must never affect a Redis call.
 */
final class ChronosPredisClient extends \Predis\Client
{
    public function __call($commandID, $arguments)
    {
        $span = self::beginSpan((string) $commandID, is_array($arguments) ? $arguments : []);
        try {
            return parent::__call($commandID, $arguments);
        } finally {
            if ($span instanceof Span) {
                try {
                    $span->finish();
                } catch (Throwable) {
                }
            }
        }
    }

    /** @param array<int,mixed> $arguments */
    private function beginSpan(string $commandID, array $arguments): ?Span
    {
        try {
            $operation = strtoupper($commandID);
            $span = SpanManager::open('redis '.$operation);
            if ($span->isVoid()) {
                return $span;
            }
            $span->add('cache.system', 'redis');
            $span->add('cache.store', 'redis');
            $span->add('db.operation', $operation);
            if (isset($arguments[0]) && is_scalar($arguments[0]) && (string) $arguments[0] !== '') {
                $span->add('db.redis.key', (string) $arguments[0]);
            }
            $this->addConnectionMetadata($span);
            [$file, $line] = self::callSite();
            if ($file !== null) {
                $span->add('code.filepath', $file);
                $span->add('code.lineno', (string) $line);
            }

            return $span;
        } catch (Throwable) {
            return null;
        }
    }

    /** Adds Redis host/db (never password) from the live connection parameters, best-effort. */
    private function addConnectionMetadata(Span $span): void
    {
        try {
            $connection = $this->getConnection();
            if (!method_exists($connection, 'getParameters')) {
                return;
            }
            $parameters = $connection->getParameters();
            $host = $parameters->host ?? null;
            if (is_scalar($host) && (string) $host !== '') {
                $port = $parameters->port ?? null;
                $span->add('cache.host', is_scalar($port) && (string) $port !== '' ? $host.':'.$port : (string) $host);
            }
            $database = $parameters->database ?? null;
            if (is_scalar($database) && (string) $database !== '') {
                $span->add('cache.db', (string) $database);
            }
        } catch (Throwable) {
        }
    }

    /**
     * @return array{0: string|null, 1: int} first application call site outside the vendor tree
     */
    private static function callSite(): array
    {
        try {
            $frames = debug_backtrace(DEBUG_BACKTRACE_IGNORE_ARGS, 30);
            foreach ($frames as $frame) {
                $file = $frame['file'] ?? null;
                if (!is_string($file) || $file === '') {
                    continue;
                }
                if (str_contains($file, '/vendor/') || str_contains($file, '/Predis/')) {
                    continue;
                }

                return [$file, (int) ($frame['line'] ?? 0)];
            }
        } catch (Throwable) {
        }

        return [null, 0];
    }
}
