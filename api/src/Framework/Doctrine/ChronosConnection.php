<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Doctrine;

use Doctrine\DBAL\Driver\Connection;
use Doctrine\DBAL\Driver\Middleware\AbstractConnectionMiddleware;
use Doctrine\DBAL\Driver\Result;
use Doctrine\DBAL\Driver\Statement;

/**
 * One span per query()/exec() call on this connection (statement-execute is
 * ChronosStatement's job — prepare() here only threads the connection metadata
 * through to it). query()/exec() carry no bound-parameter count: DBAL only ever
 * calls them with the SQL fully interpolated, so db.parameters.count is left unset
 * rather than reported as a dishonest zero.
 */
final class ChronosConnection extends AbstractConnectionMiddleware
{
    /** @param array<string, string> $metadata db.system/server.address/db.name from DoctrineMetadata */
    public function __construct(Connection $connection, private readonly array $metadata)
    {
        parent::__construct($connection);
    }

    public function prepare(string $sql): Statement
    {
        return new ChronosStatement(parent::prepare($sql), $sql, $this->metadata);
    }

    public function query(string $sql): Result
    {
        return DoctrineQuerySpan::around($sql, $this->metadata, null, fn (): Result => parent::query($sql));
    }

    public function exec(string $sql): int
    {
        return DoctrineQuerySpan::around($sql, $this->metadata, null, fn (): int => parent::exec($sql));
    }
}
