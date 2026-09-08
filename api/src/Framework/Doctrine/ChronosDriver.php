<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Doctrine;

use Doctrine\DBAL\Driver\Connection;
use Doctrine\DBAL\Driver\Middleware\AbstractDriverMiddleware;

/**
 * Only override in the Driver->Connection->Statement chain: capture the connection-params
 * array connect() receives (host/dbname/driver — never credentials) into DoctrineMetadata,
 * then hand it to ChronosConnection so every query on this connection can stamp it without
 * re-deriving it per call.
 *
 * Everything else — getDatabasePlatform(), getExceptionConverter(), and (DBAL 3 only)
 * getSchemaManager() — is left to AbstractDriverMiddleware, which forwards to the real
 * driver. Those signatures are exactly where DBAL 3 and DBAL 4 disagree (DBAL 4's
 * getDatabasePlatform() takes a ServerVersionProvider the DBAL 3 signature does not have),
 * so not touching them is what makes this one class file work unmodified against either
 * major version — the installed DBAL's own AbstractDriverMiddleware already speaks whichever
 * version's Driver interface is loaded.
 */
final class ChronosDriver extends AbstractDriverMiddleware
{
    public function connect(array $params): Connection
    {
        $connection = parent::connect($params);
        $metadata = DoctrineMetadata::fromConnectionParams($params);

        return new ChronosConnection($connection, $metadata);
    }
}
