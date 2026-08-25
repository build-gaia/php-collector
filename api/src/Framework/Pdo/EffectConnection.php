<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Pdo;

use Chronos\Collector\Replay\DatabaseAnswer;
use Chronos\Collector\Replay\Effect;
use Chronos\Collector\Replay\ReplayRuntime;
use PDO;
use PDOStatement;

/**
 * PDO decorator that answers SQL from the active replay recording.
 *
 * Bind this in the container instead of a raw PDO when the application must consume
 * MutationSweep `empty_database` (and other) fixtures without native PDO handler overrides.
 * Outside replay, every call is forwarded unchanged.
 *
 * `query` / `exec` / prepared `execute` are the seams. Result sets are returned as
 * {@see EffectStatement} (implements the PDOStatement methods apps commonly use). Code that
 * type-hints a concrete PDOStatement subclass may need a cast or interface — that is the
 * trade-off until native PDO overrides exist.
 */
final class EffectConnection
{
    public function __construct(private PDO $inner)
    {
    }

    public function inner(): PDO
    {
        return $this->inner;
    }

    public function query(string $statement, ?int $mode = null, mixed ...$fetchModeArgs): PDOStatement|EffectStatement|false
    {
        if (!ReplayRuntime::active()) {
            return $mode === null
                ? $this->inner->query($statement)
                : $this->inner->query($statement, $mode, ...$fetchModeArgs);
        }
        $payload = Effect::database($statement);
        if ($payload === null) {
            return $mode === null
                ? $this->inner->query($statement)
                : $this->inner->query($statement, $mode, ...$fetchModeArgs);
        }

        return EffectStatement::fromPayload($payload);
    }

    public function exec(string $statement): int|false
    {
        if (!ReplayRuntime::active()) {
            return $this->inner->exec($statement);
        }
        $payload = Effect::database($statement);
        if ($payload === null) {
            return $this->inner->exec($statement);
        }

        return DatabaseAnswer::rowCount($payload);
    }

    public function prepare(string $query, array $options = []): PDOStatement|EffectStatement|false
    {
        if (!ReplayRuntime::active()) {
            return $this->inner->prepare($query, $options);
        }

        return new EffectStatement($query);
    }

    public function __call(string $name, array $arguments): mixed
    {
        return $this->inner->$name(...$arguments);
    }
}
