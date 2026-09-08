<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Doctrine;

use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Throwable;

/**
 * DBAL 2 fallback: DBAL 2 predates the Driver\Middleware pipeline entirely (it arrived in
 * DBAL 3), so the only per-query seam is Configuration::setSQLLogger(). Guarded by
 * interface_exists() rather than left to PSR-4 laziness alone: unlike ChronosMiddleware
 * (never referenced unless the app's own bootstrap code already names it), a container
 * that autowires "every class implementing SQLLogger" — or a manifest that instantiates
 * this class unconditionally across a fleet running mixed DBAL versions — could reach this
 * file with DBAL 2's SQLLogger interface absent; the whole class body is skipped in that
 * case instead of fataling on an undefined interface.
 *
 * Same span shape as ChronosStatement/ChronosConnection (db.system/server.address/db.name
 * from DoctrineMetadata, db.operation/db.statement.verb/db.statement/db.query.text/
 * db.parameters.count from DoctrineQuerySpan) so a dashboard does not care which DBAL major
 * version produced a given trace.
 *
 * Usage:
 *   $config = new \Doctrine\DBAL\Configuration();
 *   $config->setSQLLogger(new ChronosSqlLogger(DoctrineMetadata::fromConnectionParams($params)));
 *   $connection = \Doctrine\DBAL\DriverManager::getConnection($params, $config);
 */
if (interface_exists('Doctrine\\DBAL\\Logging\\SQLLogger')) {
    final class ChronosSqlLogger implements \Doctrine\DBAL\Logging\SQLLogger
    {
        /** @var list<Span> nested queries (subqueries triggered from within a fetch) stack, matching startQuery/stopQuery pairing */
        private array $stack = [];

        /** @param array<string, string> $metadata db.system/server.address/db.name from DoctrineMetadata::fromConnectionParams() */
        public function __construct(private readonly array $metadata = [])
        {
        }

        public function startQuery($sql, ?array $params = null, ?array $types = null): void
        {
            try {
                $count = is_array($params) ? count($params) : DoctrineQuerySpan::countPlaceholders((string) $sql);
                $this->stack[] = DoctrineQuerySpan::open((string) $sql, $this->metadata, $count);
            } catch (Throwable) {
                // A span that fails to open must never block the query it was meant to observe.
            }
        }

        public function stopQuery(): void
        {
            $span = array_pop($this->stack);
            if (!$span instanceof Span) {
                return;
            }
            try {
                DoctrineQuerySpan::close($span);
            } catch (Throwable) {
            }
        }
    }
}
