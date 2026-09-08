<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Doctrine;

/**
 * Turns a DBAL connection-params array into the connection-identity span attributes shared by
 * every query on that connection (db.system, server.address, db.name — never a credential).
 *
 * Deliberately independent of any Doctrine\DBAL type: DBAL hands `connect()` a plain array, so
 * this needs no interface_exists() guard and no live driver to unit test, unlike everything else
 * in this directory. ChronosDriver resolves it once per connect() and threads the result through
 * ChronosConnection/ChronosStatement so a busy connection pays for this parsing once, not per query.
 */
final class DoctrineMetadata
{
    /**
     * DBAL's own driver name (the `driver` connection param, e.g. "pdo_mysql") mapped to the
     * OTel db.system value. Keys are DBAL driver names, not PDO driver names — DBAL prefixes
     * the PDO-backed ones with `pdo_`, and also ships non-PDO drivers (`mysqli`, `sqlsrv`) that
     * name the same systems.
     */
    private const SYSTEM_BY_DRIVER = [
        'pdo_mysql' => 'mysql',
        'mysqli' => 'mysql',
        'pdo_pgsql' => 'postgresql',
        'pgsql' => 'postgresql',
        'pdo_sqlite' => 'sqlite',
        'sqlite3' => 'sqlite',
        'pdo_sqlsrv' => 'mssql',
        'sqlsrv' => 'mssql',
        'pdo_oci' => 'oracle',
        'oci8' => 'oracle',
        'ibm_db2' => 'db2',
    ];

    /**
     * @param array<string, mixed> $params the array DBAL's Driver::connect() receives
     * @return array<string, string>
     */
    public static function fromConnectionParams(array $params): array
    {
        $out = [];

        $driver = is_string($params['driver'] ?? null) ? $params['driver'] : '';
        $system = self::SYSTEM_BY_DRIVER[$driver] ?? '';
        if ($system !== '') {
            $out['db.system'] = $system;
        }

        $host = $params['host'] ?? null;
        if (is_scalar($host) && (string) $host !== '') {
            $out['server.address'] = (string) $host;
            // Legacy key: the Doctrine 1 span listener already reports the peer host as
            // db.host, so a dashboard built against that name keeps working here too.
            $out['db.host'] = (string) $host;
        }

        $port = $params['port'] ?? null;
        if (is_scalar($port) && (string) $port !== '') {
            $out['server.port'] = (string) $port;
        }

        // sqlite has no host/dbname; the file path IS the database identity.
        $name = $params['dbname'] ?? ($params['path'] ?? null);
        if (is_scalar($name) && (string) $name !== '') {
            $out['db.name'] = (string) $name;
        }

        return $out;
    }
}
