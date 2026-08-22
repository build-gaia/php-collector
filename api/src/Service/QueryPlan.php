<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use PDO;
use PDOStatement;
use Throwable;

/**
 * Captures an execution plan for a SQL statement, next to the statement itself.
 *
 * WHY HERE. The Chronos engine never has a route to the observed application's
 * database — it only ever sees spans. So the only place a plan can be produced
 * is the process that already holds the connection: this one. The plan is
 * attached to the SQL span as `db.plan`, and the Database page reads it back off
 * the span index.
 *
 * WHY IT IS OFF BY DEFAULT. An EXPLAIN is a real round trip to the database on
 * the request path, and on a slow query the optimizer work is not free either.
 * Nothing here runs unless `CHRONOS_PHP_LOCAL_EXPLAIN` (or the bare
 * `CHRONOS_PHP_EXPLAIN`) is set, and even then:
 *
 *   - one capture per QUERY SHAPE per process (a shape is the statement with its
 *     literals already parameterised, so a busy endpoint explains a given query
 *     once per worker and never again),
 *   - at most MAX_PER_REQUEST captures in one request, so a request issuing 400
 *     distinct queries cannot pay 400 extra round trips,
 *   - SELECT only unless `CHRONOS_PHP_LOCAL_EXPLAIN_WRITES` is also set. EXPLAIN does
 *     not execute the statement it plans, so a write is safe to explain; it is
 *     opt-in because "the profiler touched my UPDATE" deserves an explicit yes.
 *
 * TIMING IS LOAD-BEARING. Callers MUST capture BEFORE opening the SQL span. The
 * span's duration feeds every latency number in Chronos (p95, the rollup, the
 * flame chart); an EXPLAIN inside that window would inflate all of them and the
 * corruption would be invisible. The request span does grow by the explain cost
 * — that is real time the request spent — and `db.plan.explain_millis` says how
 * much, so the overhead is measured rather than hidden.
 *
 * Every path is fail-open: a database that refuses to EXPLAIN, a driver without
 * FORMAT=JSON, a parameter set that will not bind — all of them return no plan
 * and leave the query untouched. Telemetry must never affect execution.
 */
final class QueryPlan
{
    /** Captures allowed in a single request, whatever the shape cache says. */
    private const MAX_PER_REQUEST = 5;

    /** Shapes remembered per process before the cache stops growing. */
    private const MAX_TRACKED_SHAPES = 512;

    /**
     * Both names are accepted: the `LOCAL_` prefix is the family the other
     * development-only switches use (`CHRONOS_PHP_LOCAL_RICH_TELEMETRY`), and the
     * bare name is the one anyone reaching for this reads about first.
     */
    private const ENABLE_VARS = ['CHRONOS_PHP_LOCAL_EXPLAIN', 'CHRONOS_PHP_EXPLAIN'];
    private const WRITE_VARS = ['CHRONOS_PHP_LOCAL_EXPLAIN_WRITES', 'CHRONOS_PHP_EXPLAIN_WRITES'];
    private const MAX_VARS = [
        'CHRONOS_PHP_LOCAL_EXPLAIN_MAX_PER_REQUEST',
        'CHRONOS_PHP_EXPLAIN_MAX_PER_REQUEST',
    ];

    private const READ_VERBS = ['SELECT'];
    private const WRITE_VERBS = ['INSERT', 'UPDATE', 'DELETE', 'REPLACE'];

    private static ?bool $enabled = null;
    private static ?bool $writes = null;
    private static ?int $maxPerRequest = null;
    private static int $captured = 0;

    /** @var array<string, true> shapes already explained by this process */
    private static array $seen = [];

    /**
     * Clear the per-request capture budget. Called from NativeExtension::requestStart
     * because these statics outlive a request inside an FPM worker.
     */
    public static function reset(): void
    {
        self::$captured = 0;
    }

    /** Cheap enough to call on every query: one cached bool after the first call. */
    public static function enabled(): bool
    {
        return self::$enabled ??= self::flag(self::ENABLE_VARS);
    }

    /**
     * Capture a plan, returning the span attributes to attach — an empty array
     * when nothing was captured, for any reason.
     *
     * @param array<mixed> $parameters bound parameters, in the driver's order
     *
     * @return array<string, string>
     */
    public static function capture(?PDO $pdo, string $sql, array $parameters): array
    {
        if ($pdo === null || !self::enabled()) {
            return [];
        }
        try {
            if (self::$captured >= self::maxPerRequest()) {
                return [];
            }
            $statement = trim($sql);
            if ($statement === '' || !self::explainable($statement, self::writesEnabled())) {
                return [];
            }
            $shape = self::shapeKey($statement);
            if (isset(self::$seen[$shape])) {
                return [];
            }
            $binds = self::bindable($parameters);
            if ($binds === null) {
                return [];
            }

            // Claimed before the round trip, not after: a statement the server
            // refuses to explain must not be retried on every single execution.
            if (count(self::$seen) < self::MAX_TRACKED_SHAPES) {
                self::$seen[$shape] = true;
            }
            self::$captured++;

            $started = hrtime(true);
            $plan = self::explain($pdo, $statement, $binds);
            $elapsed = (hrtime(true) - $started) / 1e6;
            if ($plan === null) {
                return [];
            }

            return self::attributes($plan[0], $plan[1], $elapsed);
        } catch (Throwable) {
            return [];
        }
    }

    /**
     * Whether this statement is one we are willing to explain. Anything that is
     * not a recognised DML verb — DDL, SET, a stored-procedure CALL, a driver's
     * own bookkeeping — is left alone.
     */
    public static function explainable(string $sql, bool $writes): bool
    {
        $verb = self::verb($sql);
        if (in_array($verb, self::READ_VERBS, true)) {
            return true;
        }

        return $writes && in_array($verb, self::WRITE_VERBS, true);
    }

    /**
     * Identity of a query SHAPE, for the once-per-process cache. Statements reach
     * here already parameterised (`?`), so collapsing whitespace and case is
     * enough to make the same query hash the same however it was formatted.
     */
    public static function shapeKey(string $sql): string
    {
        $collapsed = preg_replace('/\s+/', ' ', trim($sql));

        return substr(sha1(strtolower(is_string($collapsed) ? $collapsed : $sql)), 0, 16);
    }

    /**
     * The span attributes for a captured plan. `db.plan.source` marks where it
     * came from, so a plan captured inline is never mistaken for one produced by
     * something with its own view of the database.
     *
     * @return array<string, string>
     */
    public static function attributes(string $plan, string $format, float $explainMillis): array
    {
        return [
            'db.plan' => $plan,
            'db.plan.format' => $format,
            'db.plan.explain_millis' => number_format(max(0.0, $explainMillis), 3, '.', ''),
            'db.plan.source' => 'collector-inline',
        ];
    }

    /**
     * Run the EXPLAIN. JSON first (MySQL 5.6+, and far more informative), the
     * tabular form re-encoded as JSON when the server will not do FORMAT=JSON.
     *
     * @param list<mixed> $binds
     *
     * @return array{0: string, 1: string}|null [plan, format]
     */
    private static function explain(PDO $pdo, string $sql, array $binds): ?array
    {
        $json = self::runExplain($pdo, 'EXPLAIN FORMAT=JSON '.$sql, $binds);
        if ($json instanceof PDOStatement) {
            $column = $json->fetchColumn(0);
            if (is_string($column) && $column !== '') {
                return [$column, 'mysql-json'];
            }
        }
        $tabular = self::runExplain($pdo, 'EXPLAIN '.$sql, $binds);
        if ($tabular instanceof PDOStatement) {
            $rows = $tabular->fetchAll(PDO::FETCH_ASSOC);
            if (is_array($rows) && $rows !== []) {
                $encoded = json_encode($rows, JSON_UNESCAPED_SLASHES | JSON_PARTIAL_OUTPUT_ON_ERROR);
                if (is_string($encoded)) {
                    return [$encoded, 'mysql-table'];
                }
            }
        }

        return null;
    }

    /**
     * Prepare and execute one EXPLAIN. Returns null on anything unexpected —
     * including a driver in silent error mode, which reports failure by return
     * value rather than by throwing.
     *
     * @param list<mixed> $binds
     */
    private static function runExplain(PDO $pdo, string $sql, array $binds): ?PDOStatement
    {
        try {
            $statement = $pdo->prepare($sql);
            if (!$statement instanceof PDOStatement) {
                return null;
            }

            return $statement->execute($binds) ? $statement : null;
        } catch (Throwable) {
            return null;
        }
    }

    /**
     * Flatten bound parameters to the positional list PDO wants, or null when
     * they cannot be bound safely.
     *
     * Doctrine hands back nested arrays for some statement shapes (the SET and
     * WHERE halves of an UPDATE, for instance), and an array or object bound to a
     * placeholder would make the EXPLAIN fail — which is fine, but failing before
     * the round trip is cheaper than failing after it.
     *
     * @param array<mixed> $parameters
     *
     * @return list<mixed>|null
     */
    private static function bindable(array $parameters): ?array
    {
        $flat = [];
        foreach ($parameters as $value) {
            if (is_array($value)) {
                foreach ($value as $nested) {
                    if (!self::isBindable($nested)) {
                        return null;
                    }
                    $flat[] = self::normalise($nested);
                }
                continue;
            }
            if (!self::isBindable($value)) {
                return null;
            }
            $flat[] = self::normalise($value);
        }

        return $flat;
    }

    private static function isBindable(mixed $value): bool
    {
        return $value === null || is_scalar($value);
    }

    /** Booleans bind as integers; everything else scalar goes through as-is. */
    private static function normalise(mixed $value): mixed
    {
        return is_bool($value) ? (int) $value : $value;
    }

    private static function writesEnabled(): bool
    {
        return self::$writes ??= self::flag(self::WRITE_VARS);
    }

    private static function maxPerRequest(): int
    {
        if (self::$maxPerRequest === null) {
            $configured = self::env(self::MAX_VARS);
            $parsed = $configured === null ? self::MAX_PER_REQUEST : (int) $configured;
            self::$maxPerRequest = max(1, min(50, $parsed));
        }

        return self::$maxPerRequest;
    }

    /** @param list<string> $names */
    private static function flag(array $names): bool
    {
        $value = self::env($names);
        if ($value === null) {
            return false;
        }

        return in_array(strtolower(trim($value)), ['1', 'true', 'on', 'yes'], true);
    }

    /**
     * First non-empty value among these variables, read across getenv(), $_ENV
     * and $_SERVER — the same three places AppVersion reads, because frameworks
     * populate the environment in all three.
     *
     * @param list<string> $names
     */
    private static function env(array $names): ?string
    {
        foreach ($names as $name) {
            $value = getenv($name);
            if (is_string($value) && $value !== '') {
                return $value;
            }
            foreach ([$_ENV, $_SERVER] as $bag) {
                if (isset($bag[$name]) && is_scalar($bag[$name]) && (string) $bag[$name] !== '') {
                    return (string) $bag[$name];
                }
            }
        }

        return null;
    }

    private static function verb(string $sql): string
    {
        $trimmed = ltrim($sql);
        $space = strpos($trimmed, ' ');

        return strtoupper($space === false ? $trimmed : substr($trimmed, 0, $space));
    }
}
