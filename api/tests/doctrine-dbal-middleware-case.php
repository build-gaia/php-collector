<?php

declare(strict_types=1);

/**
 * Standalone verification for the Doctrine DBAL instrumentation (Framework\Doctrine): the
 * DBAL 3/4 Driver\Middleware chain (ChronosMiddleware/ChronosDriver/ChronosConnection/
 * ChronosStatement) and the DBAL 2 fallback (ChronosSqlLogger). See verify.php's header for
 * why this package's tests are hand-rolled scripts rather than PHPUnit.
 *
 * This script defines its own minimal Doctrine\DBAL fakes — including the
 * Driver\Middleware abstract base classes real DBAL ships — because the package installs
 * with ZERO runtime dependencies, so the test run must not require doctrine/dbal either.
 * The fakes model only the seams these five classes touch (connect/prepare/query/exec/
 * execute), not the whole DBAL surface.
 *
 * Run: php api/tests/doctrine-dbal-middleware-case.php
 */

namespace Chronos\Collector\Tests\Doctrine;

spl_autoload_register(static function (string $class): void {
    $prefix = 'Chronos\\Collector\\';
    if (!str_starts_with($class, $prefix)) {
        return;
    }
    $path = __DIR__.'/../src/'.str_replace('\\', '/', substr($class, strlen($prefix))).'.php';
    if (is_file($path)) {
        require $path;
    }
});

// --- minimal Doctrine\DBAL fakes --------------------------------------------------
//
// Matches real DBAL's own layout exactly, which is why it looks lopsided: the top-level
// Driver interface lives directly under Doctrine\DBAL (not Doctrine\DBAL\Driver — that
// path is reserved for Connection/Statement/Result/Middleware, its own children), so an
// unqualified `Driver` typehint inside namespace Doctrine\DBAL\Driver\Middleware means
// something different from one inside namespace Doctrine\DBAL\Driver.

namespace Doctrine\DBAL;

interface Driver
{
    /** @param array<string, mixed> $params */
    public function connect(array $params): \Doctrine\DBAL\Driver\Connection;
}

namespace Doctrine\DBAL\Driver;

interface Result
{
    /** @return list<array<string, mixed>> */
    public function fetchAllAssociative(): array;
}

interface Statement
{
    /** @param array<int|string, mixed>|null $params */
    public function execute($params = null): Result;
}

interface Connection
{
    public function prepare(string $sql): Statement;

    public function query(string $sql): Result;

    public function exec(string $sql): int;
}

interface Middleware
{
    public function wrap(\Doctrine\DBAL\Driver $driver): \Doctrine\DBAL\Driver;
}

namespace Doctrine\DBAL\Driver\Middleware;

use Doctrine\DBAL\Driver as DbalDriver;
use Doctrine\DBAL\Driver\Connection;
use Doctrine\DBAL\Driver\Result;
use Doctrine\DBAL\Driver\Statement;

abstract class AbstractDriverMiddleware implements DbalDriver
{
    public function __construct(private readonly DbalDriver $driver)
    {
    }

    public function connect(array $params): Connection
    {
        return $this->driver->connect($params);
    }
}

abstract class AbstractConnectionMiddleware implements Connection
{
    public function __construct(private readonly Connection $connection)
    {
    }

    public function prepare(string $sql): Statement
    {
        return $this->connection->prepare($sql);
    }

    public function query(string $sql): Result
    {
        return $this->connection->query($sql);
    }

    public function exec(string $sql): int
    {
        return $this->connection->exec($sql);
    }
}

abstract class AbstractStatementMiddleware implements Statement
{
    public function __construct(private readonly Statement $statement)
    {
    }

    public function execute($params = null): Result
    {
        return $this->statement->execute($params);
    }
}

namespace Doctrine\DBAL\Logging;

interface SQLLogger
{
    /**
     * @param array<int|string, mixed>|null $params
     * @param array<int|string, mixed>|null $types
     */
    public function startQuery($sql, ?array $params = null, ?array $types = null): void;

    public function stopQuery(): void;
}

// --- fake driver stack: records every call it receives for assertions ------------

namespace Chronos\Collector\Tests\Doctrine;

use Doctrine\DBAL\Driver as DbalDriver;
use Doctrine\DBAL\Driver\Connection;
use Doctrine\DBAL\Driver\Result;
use Doctrine\DBAL\Driver\Statement;

final class FakeResult implements Result
{
    public function fetchAllAssociative(): array
    {
        return [];
    }
}

final class FakeStatement implements Statement
{
    /** @var list<array<int|string, mixed>|null> */
    public array $executed = [];

    public function __construct(public readonly string $sql)
    {
    }

    public function execute($params = null): Result
    {
        $this->executed[] = $params;

        return new FakeResult();
    }
}

final class BoomStatement implements Statement
{
    public function execute($params = null): Result
    {
        throw new \RuntimeException('syntax error');
    }
}

final class FakeConnection implements Connection
{
    /** @var list<string> */
    public array $queried = [];

    /** @var list<string> */
    public array $executed = [];

    public ?Statement $nextStatement = null;

    public function prepare(string $sql): Statement
    {
        return $this->nextStatement ?? new FakeStatement($sql);
    }

    public function query(string $sql): Result
    {
        $this->queried[] = $sql;

        return new FakeResult();
    }

    public function exec(string $sql): int
    {
        $this->executed[] = $sql;

        return 1;
    }
}

final class FakeDriver implements DbalDriver
{
    /** @var list<array<string, mixed>> */
    public array $connectedWith = [];

    public function __construct(private readonly Connection $connection)
    {
    }

    public function connect(array $params): Connection
    {
        $this->connectedWith[] = $params;

        return $this->connection;
    }
}

// --- harness -----------------------------------------------------------------------

use Chronos\Collector\Framework\Doctrine\ChronosMiddleware;
use Chronos\Collector\Framework\Doctrine\ChronosSqlLogger;
use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Chronos\Collector\Service\TraceContext;

function beginRootSpan(): void
{
    // NativeExtension::loaded() is false in this bare test process (no .so), so
    // SpanManager::complete() falls back to its own static buffer instead of the
    // FFI bridge — exactly the pure-PHP path this test exercises.
    NativeExtension::reset();
    $root = Span::open(bin2hex(random_bytes(16)), TraceContext::newSpanId(), '', 'root');
    SpanManager::begin($root);
}

function fail(string $message): void
{
    fwrite(STDERR, "FAIL: {$message}\n");
    exit(1);
}

$connectionParams = [
    'driver' => 'pdo_mysql',
    'host' => 'db.internal.test',
    'port' => 3306,
    'dbname' => 'widgets_production',
    'user' => 'app', // never expected on a span
    'password' => 'super-secret', // never expected on a span
];

// 1. wrap()/connect(): metadata threads through to the returned Connection, and the
//    underlying driver/connection actually get called (a middleware that swallowed the
//    call instead of delegating would break every application using it).
beginRootSpan();
$innerConnection = new FakeConnection();
$innerDriver = new FakeDriver($innerConnection);
$middleware = new ChronosMiddleware();
$wrapped = $middleware->wrap($innerDriver);
$connection = $wrapped->connect($connectionParams);
if (count($innerDriver->connectedWith) !== 1 || $innerDriver->connectedWith[0] !== $connectionParams) {
    fail('ChronosDriver did not forward connect() params to the inner driver');
}

// 2. query(): one client span, db.system/server.address/db.name/db.operation/db.query.text,
//    and the real connection is actually invoked with the real SQL.
$got = $connection->query('SELECT * FROM widgets WHERE id = 1');
if ($innerConnection->queried !== ['SELECT * FROM widgets WHERE id = 1']) {
    fail('ChronosConnection::query() did not forward to the inner connection');
}
$finished = SpanManager::end();
if (count($finished) !== 1) {
    fail('expected exactly one finished span from query(), got '.count($finished));
}
$attrs = $finished[0]->attributes;
if (($attrs['db.system'] ?? null) !== 'mysql') {
    fail('missing/wrong db.system: '.var_export($attrs['db.system'] ?? null, true));
}
if (($attrs['server.address'] ?? null) !== 'db.internal.test') {
    fail('missing/wrong server.address: '.var_export($attrs['server.address'] ?? null, true));
}
if (($attrs['db.name'] ?? null) !== 'widgets_production') {
    fail('missing/wrong db.name: '.var_export($attrs['db.name'] ?? null, true));
}
if (($attrs['db.operation'] ?? null) !== 'SELECT') {
    fail('missing/wrong db.operation: '.var_export($attrs['db.operation'] ?? null, true));
}
if (($attrs['db.query.text'] ?? null) !== 'SELECT * FROM widgets WHERE id = 1') {
    fail('missing/wrong db.query.text: '.var_export($attrs['db.query.text'] ?? null, true));
}
if (isset($attrs['db.user']) || isset($attrs['user']) || str_contains(json_encode($attrs), 'super-secret')) {
    fail('a credential leaked onto the span attributes: '.json_encode($attrs));
}

// 3. prepare()->execute($params): db.parameters.count from the actual bound values, never
//    the values themselves.
beginRootSpan();
$innerConnection2 = new FakeConnection();
$statement = $connection = null; // reset for clarity
$driver2 = new ChronosMiddleware();
$connection2 = $driver2->wrap(new FakeDriver($innerConnection2))->connect($connectionParams);
$stmt = $connection2->prepare('INSERT INTO widgets (name, sku) VALUES (?, ?)');
$stmt->execute(['Left-handed widget', 'LHW-1']);
$finishedPrepare = SpanManager::end();
if (count($finishedPrepare) !== 1) {
    fail('expected exactly one finished span from prepare()->execute(), got '.count($finishedPrepare));
}
$prepareAttrs = $finishedPrepare[0]->attributes;
if (($prepareAttrs['db.parameters.count'] ?? null) !== '2') {
    fail('missing/wrong db.parameters.count: '.var_export($prepareAttrs['db.parameters.count'] ?? null, true));
}
if (str_contains(json_encode($prepareAttrs), 'Left-handed widget')) {
    fail('a bound parameter VALUE leaked onto the span, only the count is allowed: '.json_encode($prepareAttrs));
}

// 4. execute() with no $params argument still counts placeholders from the SQL text
//    (the bindValue()-per-call path DBAL apps commonly use instead of passing an array).
beginRootSpan();
$innerConnection3 = new FakeConnection();
$connection3 = (new ChronosMiddleware())->wrap(new FakeDriver($innerConnection3))->connect($connectionParams);
$stmt3 = $connection3->prepare('UPDATE widgets SET sku = :sku WHERE id = :id');
$stmt3->execute();
$finishedNoParams = SpanManager::end();
if (($finishedNoParams[0]->attributes['db.parameters.count'] ?? null) !== '2') {
    fail('expected placeholder count 2 from SQL text, got '.var_export($finishedNoParams[0]->attributes['db.parameters.count'] ?? null, true));
}

// 5. A throwing execute() still finishes its span, marked errored, and the exception
//    propagates unchanged — telemetry must never swallow it.
beginRootSpan();
$innerConnection4 = new FakeConnection();
$innerConnection4->nextStatement = new BoomStatement();
$connection4 = (new ChronosMiddleware())->wrap(new FakeDriver($innerConnection4))->connect($connectionParams);
$stmt4 = $connection4->prepare('DELETE FROM widgets WHERE id = 1');
$caught = null;
try {
    $stmt4->execute();
} catch (\RuntimeException $e) {
    $caught = $e;
}
if ($caught === null || $caught->getMessage() !== 'syntax error') {
    fail('exception from execute() was not rethrown unchanged');
}
$finishedError = SpanManager::end();
if (count($finishedError) !== 1 || $finishedError[0]->status !== 'error') {
    fail('expected exactly one errored span from a throwing execute()');
}

// 6. exec(): forwarded, spanned, no parameter count (DBAL never calls exec() with bindings).
beginRootSpan();
$innerConnection5 = new FakeConnection();
$connection5 = (new ChronosMiddleware())->wrap(new FakeDriver($innerConnection5))->connect($connectionParams);
$connection5->exec('DELETE FROM widgets WHERE archived = 1');
if ($innerConnection5->executed !== ['DELETE FROM widgets WHERE archived = 1']) {
    fail('ChronosConnection::exec() did not forward to the inner connection');
}
$finishedExec = SpanManager::end();
if (($finishedExec[0]->attributes['db.operation'] ?? null) !== 'DELETE') {
    fail('missing/wrong db.operation on exec(): '.var_export($finishedExec[0]->attributes['db.operation'] ?? null, true));
}
if (isset($finishedExec[0]->attributes['db.parameters.count'])) {
    fail('exec() must never report a parameter count');
}

// 7. DBAL 2 fallback: ChronosSqlLogger pairs startQuery()/stopQuery() into the same span
//    shape, and is defined at all because this test's fake SQLLogger interface exists.
if (!interface_exists('Doctrine\\DBAL\\Logging\\SQLLogger')) {
    fail('test setup bug: fake Doctrine\\DBAL\\Logging\\SQLLogger did not load');
}
if (!class_exists(ChronosSqlLogger::class)) {
    fail('ChronosSqlLogger was not defined even though SQLLogger exists');
}
beginRootSpan();
$logger = new ChronosSqlLogger(['db.system' => 'mysql', 'server.address' => 'db.internal.test']);
$logger->startQuery('SELECT * FROM widgets WHERE id = ?', [1]);
$logger->stopQuery();
$finishedLogger = SpanManager::end();
if (count($finishedLogger) !== 1) {
    fail('expected exactly one finished span from the SQLLogger, got '.count($finishedLogger));
}
$loggerAttrs = $finishedLogger[0]->attributes;
if (($loggerAttrs['db.operation'] ?? null) !== 'SELECT') {
    fail('missing/wrong db.operation from ChronosSqlLogger: '.var_export($loggerAttrs['db.operation'] ?? null, true));
}
if (($loggerAttrs['db.system'] ?? null) !== 'mysql') {
    fail('ChronosSqlLogger did not carry the metadata passed to its constructor');
}
if (($loggerAttrs['db.parameters.count'] ?? null) !== '1') {
    fail('missing/wrong db.parameters.count from ChronosSqlLogger: '.var_export($loggerAttrs['db.parameters.count'] ?? null, true));
}

// 8. ChronosSqlLogger: nested startQuery()/stopQuery() pairs (a query issued from inside
//    another's fetch, e.g. lazy-loaded association) unwind in stack order, not confused
//    with each other.
beginRootSpan();
$logger2 = new ChronosSqlLogger([]);
$logger2->startQuery('SELECT * FROM widgets');
$logger2->startQuery('SELECT * FROM widget_variants WHERE widget_id = ?', [1]);
$logger2->stopQuery(); // closes the inner (variants) query
$logger2->stopQuery(); // closes the outer (widgets) query
$finishedNested = SpanManager::end();
if (count($finishedNested) !== 2) {
    fail('expected exactly two finished spans from nested SQLLogger calls, got '.count($finishedNested));
}

echo "OK: Doctrine DBAL middleware + ChronosSqlLogger (8 cases)\n";
