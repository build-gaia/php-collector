<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony1;

use Chronos\Collector\Service\QueryPlan;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Throwable;

/**
 * Doctrine 1 connection listener that turns each executed SQL statement into a child span under
 * the current request span. Only loaded for Symfony 1 apps that actually have Doctrine present
 * (ChronosFilter guards on class_exists Doctrine_Manager), so extending Doctrine_EventListener is
 * safe. Every callback is fully defensive: telemetry must never affect query execution.
 */
final class DoctrineSpanListener extends \Doctrine_EventListener
{
    /** @var list<Span|null> */
    private array $stack = [];

    public function preStmtExecute(\Doctrine_Event $event): void
    {
        $this->open($event);
    }

    public function postStmtExecute(\Doctrine_Event $event): void
    {
        $this->close();
    }

    public function preExec(\Doctrine_Event $event): void
    {
        $this->open($event);
    }

    public function postExec(\Doctrine_Event $event): void
    {
        $this->close();
    }

    private function open(\Doctrine_Event $event): void
    {
        try {
            $sql = (string) $event->getQuery();
            $eventParams = method_exists($event, 'getParams') ? $event->getParams() : null;
            // BEFORE the span opens, deliberately. An EXPLAIN is a round trip, and
            // inside the span window it would be counted as query time — every
            // latency number Chronos reports for this query would then include the
            // cost of profiling it. Off unless CHRONOS_PHP_EXPLAIN is set, so this
            // is one static bool check on the normal path. See QueryPlan.
            $plan = QueryPlan::enabled()
                ? QueryPlan::capture(
                    self::pdo(self::resolveConnection($event)),
                    $sql,
                    is_array($eventParams) ? $eventParams : [],
                )
                : [];
            $span = SpanManager::open('SQL ' . self::verb($sql));
            if (!$span->isVoid()) {
                $span->add('span.kind', 'client');
                $span->add('db.system', 'mysql');
                $span->add('db.statement.verb', self::verb($sql));
                self::addConnectionMetadata($span, $event);
                [$file, $line, $function] = self::callSite();
                if ($file !== null) {
                    $span->add('code.filepath', $file);
                    $span->add('code.lineno', (string) $line);
                }
                if ($function !== null) {
                    $span->add('code.function', $function);
                }
                // Full statement, bound parameter count, and bound parameter values are
                // human-readable; keep them behind the explicit local rich-telemetry flag,
                // never on in production. The statement and its parameter JSON opt into the
                // larger text ceiling so a big query is captured whole, not clipped at 512.
                $span->add('db.statement', $sql, Span::MAX_TEXT_LENGTH);
                $span->add('db.query.text', $sql, Span::MAX_TEXT_LENGTH);
                if (is_array($eventParams)) {
                    $span->add('db.parameters.count', (string) count($eventParams));
                    $span->add('db.parameters', Span::boundedParametersJson($eventParams), Span::MAX_TEXT_LENGTH);
                }
                // The plan, when one was captured. A plan is large and structured,
                // so it opts into the same text ceiling as the statement.
                foreach ($plan as $key => $value) {
                    $span->add($key, $value, Span::MAX_TEXT_LENGTH);
                }
            }
            $this->stack[] = $span;
        } catch (Throwable) {
            $this->stack[] = null;
        }
    }

    private function close(): void
    {
        $span = array_pop($this->stack);
        if ($span instanceof Span) {
            try {
                $span->finish();
            } catch (Throwable) {
            }
        }
    }

    /**
     * The connection behind an event.
     *
     * Doctrine 1's Doctrine_Event has NO getConnection() — the connection arrives
     * via getInvoker(), which is either the connection itself or a
     * Doctrine_Connection_Statement exposing getConnection(). The old
     * method_exists($event, 'getConnection') guard silently returned on every
     * query, which is why SQL nodes showed "host not reported".
     */
    private static function resolveConnection(\Doctrine_Event $event): ?object
    {
        try {
            if (method_exists($event, 'getInvoker')) {
                $invoker = $event->getInvoker();
                if ($invoker instanceof \Doctrine_Connection) {
                    return $invoker;
                }
                if (is_object($invoker) && method_exists($invoker, 'getConnection')) {
                    $connection = $invoker->getConnection();
                    if (is_object($connection)) {
                        return $connection;
                    }
                }
            }
            if (method_exists($event, 'getConnection')) {
                $connection = $event->getConnection();

                return is_object($connection) ? $connection : null;
            }
        } catch (Throwable) {
        }

        return null;
    }

    /**
     * The live PDO handle behind a Doctrine connection, for the EXPLAIN. Doctrine
     * connects lazily, so this can open the connection — harmless here, because
     * the statement it is about to explain is about to run on it anyway.
     */
    private static function pdo(?object $connection): ?\PDO
    {
        try {
            if ($connection === null || !method_exists($connection, 'getDbh')) {
                return null;
            }
            $handle = $connection->getDbh();

            return $handle instanceof \PDO ? $handle : null;
        } catch (Throwable) {
            return null;
        }
    }

    /** Adds DB host/name/user (never password) from the Doctrine connection, best-effort. */
    private static function addConnectionMetadata(Span $span, \Doctrine_Event $event): void
    {
        try {
            $connection = self::resolveConnection($event);
            if ($connection === null || !method_exists($connection, 'getOption')) {
                return;
            }
            $user = $connection->getOption('username');
            $host = null;
            $name = null;
            $dsn = $connection->getOption('dsn');
            // Doctrine 1 may hand back either a parsed DSN array or the raw DSN string, and the
            // string comes in two shapes: URL style (mysql://user:pass@host:port/dbname) or PDO
            // style (mysql:host=...;dbname=...). Handle all three; never let parsing throw.
            if (is_array($dsn)) {
                $host = $dsn['host'] ?? null;
                $name = $dsn['dbname'] ?? $dsn['database'] ?? null;
                $user ??= $dsn['user'] ?? $dsn['username'] ?? null;
            } elseif (is_string($dsn) && $dsn !== '') {
                $parts = @parse_url($dsn);
                if (is_array($parts) && isset($parts['host'])) {
                    $host = $parts['host'];
                    $name = isset($parts['path']) ? ltrim((string) $parts['path'], '/') : null;
                    $user ??= $parts['user'] ?? null;
                } else {
                    if (preg_match('/host=([^;]+)/', $dsn, $m) === 1) {
                        $host = $m[1];
                    }
                    if (preg_match('/dbname=([^;]+)/', $dsn, $m) === 1) {
                        $name = $m[1];
                    }
                }
            }
            if (is_scalar($host) && (string) $host !== '') {
                $span->add('db.host', (string) $host);
                // server.address is the OTel semconv key for the peer host on a client span.
                $span->add('server.address', (string) $host);
            }
            if (is_scalar($name) && (string) $name !== '') {
                $span->add('db.name', (string) $name);
            }
            if (is_scalar($user) && (string) $user !== '') {
                $span->add('db.user', (string) $user);
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
            foreach ($frames as $index => $frame) {
                $file = $frame['file'] ?? null;
                if (!is_string($file) || $file === '') {
                    continue;
                }
                if (str_contains($file, '/vendor/') || str_contains($file, '/Doctrine')) {
                    continue;
                }

                return [$file, (int) ($frame['line'] ?? 0), self::frameFunction($frames[$index + 1] ?? null)];
            }
        } catch (Throwable) {
        }

        return [null, 0, null];
    }

    /** @param array<string, mixed>|null $frame */
    private static function frameFunction(?array $frame): ?string
    {
        if ($frame === null) {
            return null;
        }
        $function = is_string($frame['function'] ?? null) ? $frame['function'] : '';
        if ($function === '') {
            return null;
        }
        $class = is_string($frame['class'] ?? null) ? $frame['class'] : '';

        return $class === '' ? $function : $class . '::' . $function;
    }

    private static function verb(string $sql): string
    {
        $trimmed = ltrim($sql);
        $space = strpos($trimmed, ' ');
        $first = $space === false ? $trimmed : substr($trimmed, 0, $space);

        return strtoupper($first) ?: 'QUERY';
    }
}
