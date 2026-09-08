<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Doctrine;

use Doctrine\DBAL\Driver\Middleware\AbstractStatementMiddleware;
use Doctrine\DBAL\Driver\Result;
use Doctrine\DBAL\Driver\Statement;

/**
 * One span per prepared-statement execute(). The bound-parameter COUNT (never values)
 * comes from the SQL text's own placeholders (DoctrineQuerySpan::countPlaceholders),
 * not from intercepting bindValue()/bindParam() — see that method's docblock for why:
 * DBAL 3's Statement::bindValue() returns bool where DBAL 4's returns void, an
 * incompatible override either way, while every version still calls this execute()
 * with the same prepared SQL string available from the constructor.
 *
 * execute()'s own $params argument, when present, is the exact bound values for
 * this one call and is preferred over the text-derived count when given.
 */
final class ChronosStatement extends AbstractStatementMiddleware
{
    /** @param array<string, string> $metadata db.system/server.address/db.name from DoctrineMetadata */
    public function __construct(
        Statement $statement,
        private readonly string $sql,
        private readonly array $metadata,
    ) {
        parent::__construct($statement);
    }

    public function execute($params = null): Result
    {
        $parameterCount = is_array($params)
            ? count($params)
            : DoctrineQuerySpan::countPlaceholders($this->sql);

        return DoctrineQuerySpan::around(
            $this->sql,
            $this->metadata,
            $parameterCount,
            fn (): Result => parent::execute($params),
        );
    }
}
