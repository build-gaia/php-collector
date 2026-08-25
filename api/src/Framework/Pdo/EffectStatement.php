<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Pdo;

use Chronos\Collector\Replay\DatabaseAnswer;
use Chronos\Collector\Replay\Effect;
use Chronos\Collector\Replay\ReplayRuntime;
use PDO;

/**
 * Replay-backed statement: either a completed result set, or a prepared shell that resolves
 * on {@see self::execute()}.
 */
final class EffectStatement
{
    private string $statement;

    /** @var list<array<string, mixed>> */
    private array $rows;

    private int $rowCount;

    private int $cursor = 0;

    private bool $resolved;

    /**
     * @param list<array<string, mixed>> $rows
     */
    public function __construct(string $statement = '', array $rows = [], int $rowCount = 0, bool $resolved = false)
    {
        $this->statement = $statement;
        $this->rows = $rows;
        $this->rowCount = $rowCount;
        $this->resolved = $resolved;
    }

    /**
     * @param array<string, mixed> $payload
     */
    public static function fromPayload(array $payload): self
    {
        $rows = DatabaseAnswer::rows($payload);

        return new self('', $rows, DatabaseAnswer::rowCount($payload), true);
    }

    public function execute(?array $params = null): bool
    {
        if ($this->resolved) {
            $this->cursor = 0;

            return true;
        }
        if (!ReplayRuntime::active()) {
            return false;
        }
        $payload = Effect::database($this->statement);
        if ($payload === null) {
            return false;
        }
        $this->rows = DatabaseAnswer::rows($payload);
        $this->rowCount = DatabaseAnswer::rowCount($payload);
        $this->resolved = true;
        $this->cursor = 0;

        return true;
    }

    /** @return list<array<string, mixed>> */
    public function fetchAll(int $mode = PDO::FETCH_ASSOC, mixed ...$args): array
    {
        return $this->rows;
    }

    public function fetch(int $mode = PDO::FETCH_ASSOC, int $cursorOrientation = PDO::FETCH_ORI_NEXT, int $cursorOffset = 0): mixed
    {
        if (!isset($this->rows[$this->cursor])) {
            return false;
        }

        return $this->rows[$this->cursor++];
    }

    public function rowCount(): int
    {
        return $this->rowCount;
    }

    public function columnCount(): int
    {
        $first = $this->rows[0] ?? null;

        return is_array($first) ? count($first) : 0;
    }

    public function closeCursor(): bool
    {
        $this->cursor = count($this->rows);

        return true;
    }
}
