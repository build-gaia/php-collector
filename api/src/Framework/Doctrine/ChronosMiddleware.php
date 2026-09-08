<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Doctrine;

use Doctrine\DBAL\Driver;
use Doctrine\DBAL\Driver\Middleware;

/**
 * DBAL 3/4 entry point: wraps the application's real Driver in ChronosDriver so every
 * connection it opens carries a span-emitting Connection/Statement pair.
 *
 * No interface_exists() guard here (unlike ChronosSqlLogger): this class, ChronosDriver,
 * ChronosConnection and ChronosStatement are only ever reached by an application that
 * already constructs a Doctrine\DBAL\Configuration and calls setMiddlewares() with this
 * class in the list — the same PSR-4 laziness argument as ChronosHttpClient/
 * ChronosPredisClient (Framework\HttpClient, Framework\Predis): an app with no DBAL 3/4
 * installed has no code path that autoloads this file at all, so composer.json needs no
 * doctrine/dbal "require" for the package to stay zero-dependency.
 *
 * Usage — plain PHP:
 *   $config = new \Doctrine\DBAL\Configuration();
 *   $config->setMiddlewares([new ChronosMiddleware()]);
 *   $connection = \Doctrine\DBAL\DriverManager::getConnection($params, $config);
 *
 * Usage — Symfony (doctrine-bundle): tag the service doctrine.middleware, see the wiring
 * instructions returned alongside this file.
 */
final class ChronosMiddleware implements Middleware
{
    public function wrap(Driver $driver): Driver
    {
        return new ChronosDriver($driver);
    }
}
