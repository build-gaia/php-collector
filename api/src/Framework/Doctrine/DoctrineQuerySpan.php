<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Doctrine;

use Chronos\Collector\Service\CallSite;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Throwable;

/**
 * Opens, fills in, and closes the client span around one query/exec/statement-execute call.
 *
 * This is the one place that knows the span SHAPE (the attribute vocabulary shared with the
 * Doctrine 1 listener: db.statement.verb/db.host alongside the current db.operation/server.address,
 * so an existing dashboard built against the legacy keys keeps working). Everything that decorates
 * a DBAL Driver — ChronosConnection for query()/exec(), ChronosStatement for execute(), and the
 * DBAL 2 ChronosSqlLogger fallback — calls through here instead of duplicating it, which is also
 * why this class carries no Doctrine\DBAL type: those three call sites disagree on what a "result"
 * even is (a DBAL 3/4 Result object vs a row count vs nothing at all for the logger), so the span
 * bookkeeping is factored out from the value being produced.
 *
 * Deliberately independent of any Doctrine\DBAL type for a second reason: it is the only class in
 * this directory a unit test can exercise without a live DBAL installation to construct against.
 */
final class DoctrineQuerySpan
{
    /**
     * @template T
     * @param array<string, string> $metadata db.system/server.address/db.name from DoctrineMetadata
     * @param callable(): T $execute the real call — parent::query()/exec()/execute()
     * @return T
     */
    public static function around(string $sql, array $metadata, ?int $parameterCount, callable $execute): mixed
    {
        $span = self::open($sql, $metadata, $parameterCount);
        try {
            return $execute();
        } catch (Throwable $exception) {
            // A query that throws still gets its span, marked so it shows up as the failure
            // it was — telemetry must never swallow the exception, only observe it.
            $span->markError();

            throw $exception;
        } finally {
            $span->finish();
        }
    }

    /**
     * Split out of around() for ChronosSqlLogger (the DBAL 2 fallback): SQLLogger's
     * startQuery()/stopQuery() are two separate callback invocations with no callable
     * spanning both, so that caller cannot use around() and instead opens here, then
     * closes with close() from its own stopQuery().
     *
     * @param array<string, string> $metadata
     */
    public static function open(string $sql, array $metadata, ?int $parameterCount): Span
    {
        $verb = self::verb($sql);
        $span = SpanManager::open('SQL '.$verb);
        if (!$span->isVoid()) {
            self::fill($span, $sql, $verb, $metadata, $parameterCount);
        }

        return $span;
    }

    /** Counterpart to open(); mirrors around()'s finally/catch behaviour for a caller with no exception to report. */
    public static function close(Span $span): void
    {
        $span->finish();
    }

    /** @param array<string, string> $metadata */
    private static function fill(Span $span, string $sql, string $verb, array $metadata, ?int $parameterCount): void
    {
        $span->add('span.kind', 'client');
        foreach ($metadata as $key => $value) {
            if ($value !== '') {
                $span->add($key, $value);
            }
        }
        $span->add('db.operation', $verb);
        $span->add('db.statement.verb', $verb); // legacy key, matches the Doctrine 1 listener
        // Full statement text is human-readable; it opts into the larger text ceiling so a big
        // query is captured whole rather than clipped at the generic 512-byte attribute bound.
        $span->add('db.statement', $sql, Span::MAX_TEXT_LENGTH);
        $span->add('db.query.text', $sql, Span::MAX_TEXT_LENGTH);
        if ($parameterCount !== null) {
            // COUNT only — never the bound values, which may carry user data.
            $span->add('db.parameters.count', (string) $parameterCount);
        }
        CallSite::applyToSpan($span);
    }

    /**
     * Number of bound-parameter placeholders in a SQL string: positional `?` outside a quoted
     * literal, or named `:token` placeholders. Counted from the TEXT rather than by intercepting
     * bindValue()/bindParam(), because those two methods are exactly where DBAL 3 and DBAL 4
     * disagree on the wire (DBAL 4's Statement::bindValue() returns void where DBAL 3's returns
     * bool — an incompatible override either way), while every version hands ChronosStatement the
     * same SQL string at prepare() time. A count from the text can overcount a `?` that legitimately
     * appears inside a string literal the naive scan does not track as a literal; that only inflates
     * the count, and this is an observability number, not something query execution depends on.
     */
    public static function countPlaceholders(string $sql): int
    {
        $withoutLiterals = preg_replace("/'(?:[^'\\\\]|\\\\.)*'/", '', $sql);
        $withoutLiterals = is_string($withoutLiterals) ? $withoutLiterals : $sql;
        $positional = substr_count($withoutLiterals, '?');
        $named = preg_match_all('/:[A-Za-z_][A-Za-z0-9_]*/', $withoutLiterals);

        return $positional + (is_int($named) ? $named : 0);
    }

    /** Shared with ChronosSqlLogger, which has no span-per-call helper of its own to reuse this from. */
    public static function verb(string $sql): string
    {
        $trimmed = ltrim($sql);
        $space = strpos($trimmed, ' ');
        $first = $space === false ? $trimmed : substr($trimmed, 0, $space);

        return strtoupper($first) ?: 'QUERY';
    }
}
