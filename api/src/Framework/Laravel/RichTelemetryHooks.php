<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\AppVersion;
use Chronos\Collector\Service\CacheCapture;
use Chronos\Collector\Service\CallSite;
use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\QueryPlan;
use Chronos\Collector\Service\Severity;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Illuminate\Cache\Events\CacheHit;
use Illuminate\Cache\Events\CacheMissed;
use Illuminate\Cache\Events\KeyForgotten;
use Illuminate\Cache\Events\KeyWritten;
use Illuminate\Database\Events\TransactionBeginning;
use Illuminate\Database\Events\TransactionCommitted;
use Illuminate\Database\Events\TransactionRolledBack;
use Illuminate\Http\Client\Events\RequestSending;
use Illuminate\Http\Client\Events\ResponseReceived;
use Illuminate\Redis\Events\CommandExecuted;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\Event;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Facades\Log;
use Throwable;

/**
 * Registers instrumentation listeners at most once per process. Creates child
 * spans for DB queries, cache reads/writes/forgets, and outbound HTTP. Forwards
 * application log messages to the native extension. Laravel request facts
 * (views, models, mail, jobs, gates, events) accumulate on RequestFacts and
 * are stamped on the request root at flush. When DST is armed, also emits the
 * matching effect kinds — Laravel suppresses native SQL/cache observers, so
 * without this the recording would only contain time/random/env builtins.
 */
final class RichTelemetryHooks
{
    private static bool $installed = false;

    public static function install(): void
    {
        if (self::$installed || !NativeExtension::loaded()) {
            return;
        }

        try {
            if (class_exists(DB::class)) {
                DB::listen(static function (object $query): void {
                    self::traceDatabaseQuery($query);
                });
            }
            if (class_exists(Event::class) && class_exists(TransactionBeginning::class)) {
                Event::listen(TransactionBeginning::class, static function (object $event): void {
                    self::openTransaction($event);
                });
                Event::listen(TransactionCommitted::class, static function (object $event): void {
                    self::closeTransaction($event, 'commit');
                });
                Event::listen(TransactionRolledBack::class, static function (object $event): void {
                    self::closeTransaction($event, 'rollback');
                });
            }
            if (class_exists(Log::class)) {
                Log::listen(static function (object $event): void {
                    self::recordLog($event);
                });
            } elseif (class_exists(Event::class) && class_exists(\Illuminate\Log\Events\MessageLogged::class)) {
                Event::listen(\Illuminate\Log\Events\MessageLogged::class, static function (object $event): void {
                    self::recordLog($event);
                });
            }
            if (class_exists(Event::class) && class_exists(CacheHit::class)) {
                Event::listen(CacheHit::class, static function (object $event): void {
                    self::traceCacheRead($event);
                });
            }
            if (class_exists(Event::class) && class_exists(CacheMissed::class)) {
                Event::listen(CacheMissed::class, static function (object $event): void {
                    self::traceCacheRead($event);
                });
            }
            if (class_exists(Event::class) && class_exists(KeyWritten::class)) {
                Event::listen(KeyWritten::class, static function (object $event): void {
                    self::traceCacheWrite($event);
                });
            }
            if (class_exists(Event::class) && class_exists(KeyForgotten::class)) {
                Event::listen(KeyForgotten::class, static function (object $event): void {
                    self::traceCacheForget($event);
                });
            }
            if (class_exists(Event::class) && class_exists(CommandExecuted::class)) {
                Event::listen(CommandExecuted::class, static function (object $event): void {
                    self::traceRedisCommand($event);
                });
            }
            // Prefer Laravel HTTP client events: Facade::method_exists is false for
            // Http::globalMiddleware (magic __callStatic), so that gate never armed DST
            // or spans for outbound calls. Keep the middleware path as a fallback.
            if (class_exists(Event::class) && class_exists(RequestSending::class)) {
                Event::listen(RequestSending::class, static function (object $event): void {
                    self::traceHttpRequestSending($event);
                });
                if (class_exists(ResponseReceived::class)) {
                    Event::listen(ResponseReceived::class, static function (object $event): void {
                        self::traceHttpResponseReceived($event);
                    });
                }
            } elseif (class_exists(Http::class) && is_callable([Http::class, 'globalMiddleware'])) {
                self::traceOutboundHttp();
            }
            RequestFacts::listen();
            self::$installed = true;
        } catch (Throwable) {
        }
    }

    private static function traceDatabaseQuery(object $query): void
    {
        $sql = (string) ($query->sql ?? '');
        $span = SpanManager::open('SQL ' . self::sqlVerb($sql));
        $span->backdateStart((float) ($query->time ?? 0.0));
        if (!$span->isVoid()) {
            $span->add('span.kind', 'client');
            $span->add('db.system', 'sql');
            self::addConnectionMetadata($span, $query);
            [$file, $line, $function] = self::callSite();
            if ($file !== null) {
                $span->add('code.filepath', $file);
                $span->add('code.lineno', (string) $line);
            }
            if ($function !== null) {
                $span->add('code.function', $function);
            }
            $span->add('db.statement', $sql, Span::MAX_TEXT_LENGTH);
            $span->add('db.query.text', $sql, Span::MAX_TEXT_LENGTH);
            $bindingsCount = count((array) ($query->bindings ?? []));
            $span->add('db.parameters.count', (string) $bindingsCount);
            $span->add('db.parameters', Span::boundedParametersJson((array) ($query->bindings ?? [])), Span::MAX_TEXT_LENGTH);
            // DB::listen fires AFTER execution and this span's duration comes from
            // $query->time, so an EXPLAIN here cannot contaminate the query's own
            // timing — unlike the Doctrine path, which has to capture before the
            // span opens. Off unless CHRONOS_PHP_EXPLAIN is set. See QueryPlan.
            if (QueryPlan::enabled()) {
                foreach (QueryPlan::capture(self::pdo($query), $sql, (array) ($query->bindings ?? [])) as $key => $value) {
                    $span->add($key, $value, Span::MAX_TEXT_LENGTH);
                }
            }
        }
        $span->finish();
        // Pair for replay: query then a result placeholder (row bodies stay privacy-gated).
        $site = self::dstCallSitePayload($span);
        NativeExtension::recordDstEffect('database_query', array_merge([
            'statement' => self::clip($sql, 4096),
        ], $site));
        NativeExtension::recordDstEffect('database_result', array_merge([
            'statement' => self::clip($sql, 4096),
            'duration_ms' => (string) ($query->time ?? 0),
        ], $site));
    }

    /**
     * The duration of the last Redis command the cache layer issued, in
     * milliseconds, waiting to be claimed by the cache event that follows it.
     *
     * Laravel raises the two halves of one cache operation separately: the Redis
     * event knows what the round trip COST, the cache event knows what it MEANT
     * (hit or miss, the logical key, the store). Neither alone is the span a
     * reader wants, so the cost is parked here for the few microseconds until the
     * cache event claims it.
     */
    private static ?float $lastCacheCommandMs = null;

    /**
     * One Redis round trip, from Laravel's own command event.
     *
     * `suppressNative('cache')` stands the native Redis observer down for the
     * whole request (observer.rs `cache_io_method`), because the userland cache
     * spans carry hit/miss and the logical key the .so cannot see. That trade
     * silently took every NON-cache Redis command with it — the facade, locks,
     * sessions, the rate limiter, Horizon — because Laravel's cache events only
     * fire for `Cache::*`. This listener puts them back, carrying the real
     * round-trip duration.
     *
     * Commands the cache layer issued are not spanned twice. `RedisStore`
     * prefixes every key it touches with `cache.prefix`, which makes the prefix
     * an exact discriminator; their duration is handed to the cache span instead,
     * so one operation stays one span and gains the timing it never had.
     */
    private static function traceRedisCommand(object $event): void
    {
        try {
            $command = strtoupper((string) ($event->command ?? ''));
            if ($command === '') {
                return;
            }
            $parameters = is_array($event->parameters ?? null) ? $event->parameters : [];
            $durationMs = is_numeric($event->time ?? null) ? (float) $event->time : null;
            $connection = is_string($event->connectionName ?? null) ? $event->connectionName : '';
            if (self::isCacheLayerCommand($parameters, $connection)) {
                self::$lastCacheCommandMs = $durationMs;

                return;
            }
            $span = SpanManager::open('REDIS '.$command);
            if ($durationMs !== null) {
                $span->backdateStart($durationMs);
            }
            if (!$span->isVoid()) {
                $span->add('span.kind', 'client');
                $span->add('db.system', 'redis');
                $span->add('db.operation', $command);
                if ($connection !== '') {
                    $span->add('db.redis.connection', $connection);
                }
                $key = self::redisKey($parameters);
                if ($key !== '') {
                    $span->add('db.redis.key', $key);
                }
                $span->add('db.parameters.count', (string) count($parameters));
                self::addRedisConnectionMetadata($span, $connection);
                [$file, $line, $function] = self::callSite();
                if ($file !== null) {
                    $span->add('code.filepath', $file);
                    $span->add('code.lineno', (string) $line);
                }
                if ($function !== null) {
                    $span->add('code.function', $function);
                }
            }
            $span->finish();
        } catch (Throwable) {
        }
    }

    /**
     * Whether this command came from the cache layer, and so is already
     * represented by a cache-event span.
     *
     * The key prefix is the discriminator because it is the thing `RedisStore`
     * actually does to every key. When no prefix is configured it cannot
     * discriminate at all — an empty prefix matches everything — so it falls back
     * to the connection the cache store is configured to use.
     */
    private static function isCacheLayerCommand(array $parameters, string $connection): bool
    {
        try {
            if (!function_exists('config')) {
                return false;
            }
            $prefix = config('cache.prefix');
            if (is_string($prefix) && $prefix !== '') {
                $key = self::redisKey($parameters);

                return $key !== '' && str_starts_with($key, $prefix);
            }

            return $connection !== '' && $connection === self::cacheConnectionName();
        } catch (Throwable) {
            return false;
        }
    }

    /** The connection name the default cache store is configured against. */
    private static function cacheConnectionName(): string
    {
        try {
            $store = config('cache.default');
            if (!is_string($store) || $store === '') {
                return '';
            }
            $connection = config("cache.stores.{$store}.connection");

            return is_string($connection) ? $connection : '';
        } catch (Throwable) {
            return '';
        }
    }

    /**
     * The key a Redis command operated on: its first argument, or the first
     * member when that argument is itself a key list (`MGET`, `DEL`).
     */
    private static function redisKey(array $parameters): string
    {
        $first = $parameters[0] ?? null;
        if (is_array($first)) {
            $first = $first[0] ?? (array_key_first($first) !== null ? array_key_first($first) : null);
        }

        return is_scalar($first) ? self::clip((string) $first, 256) : '';
    }

    /** Host and database for a named Redis connection, for the service map. */
    private static function addRedisConnectionMetadata(object $span, string $connection): void
    {
        try {
            if ($connection === '' || !function_exists('config')) {
                return;
            }
            foreach (['cache.host' => 'host', 'cache.db' => 'database'] as $attr => $key) {
                $value = config("database.redis.{$connection}.{$key}");
                if (is_scalar($value) && (string) $value !== '') {
                    $span->add($attr, (string) $value);
                    if ($attr === 'cache.host') {
                        $span->add('server.address', (string) $value);
                    }
                }
            }
        } catch (Throwable) {
        }
    }

    /**
     * Claim the round-trip duration parked by the Redis command this cache event
     * describes, so the cache span shows what it cost rather than reading as
     * instantaneous. Null when the store is not Redis-backed (file, array,
     * database), where there was no round trip to time.
     */
    private static function claimCacheCommandMs(): ?float
    {
        $duration = self::$lastCacheCommandMs;
        self::$lastCacheCommandMs = null;

        return $duration;
    }

    /**
     * Open transaction spans, innermost last.
     *
     * Held OPEN for the life of the transaction rather than stamped after the
     * fact, which is what makes the queries inside nest under it: SpanManager
     * parents every new span to the top of the stack, so a transaction that is on
     * the stack adopts its own statements. That nesting is the whole point — forty
     * loose sibling queries and forty queries under one BEGIN read very
     * differently when you are looking for a lock held too long.
     *
     * @var list<Span>
     */
    private static array $transactionSpans = [];

    /**
     * A transaction, or a savepoint inside one.
     *
     * Laravel raises the same event for both and distinguishes them by
     * `transactionLevel()`, so the level is recorded rather than flattened: a
     * rollback to a savepoint is a different event from a rollback of the
     * transaction, and a trace that showed them alike would hide the one that
     * matters.
     */
    private static function openTransaction(object $event): void
    {
        try {
            $level = self::transactionLevel($event);
            $span = SpanManager::open($level > 1 ? 'SQL SAVEPOINT' : 'SQL TRANSACTION');
            if (!$span->isVoid()) {
                $span->add('span.kind', 'client');
                $span->add('db.system', 'sql');
                $span->add('db.operation', $level > 1 ? 'SAVEPOINT' : 'TRANSACTION');
                $span->add('db.transaction.level', (string) $level);
                self::addConnectionMetadata($span, $event);
                [$file, $line, $function] = self::callSite();
                if ($file !== null) {
                    $span->add('code.filepath', $file);
                    $span->add('code.lineno', (string) $line);
                }
                if ($function !== null) {
                    $span->add('code.function', $function);
                }
            }
            self::$transactionSpans[] = $span;
        } catch (Throwable) {
        }
    }

    /** Close the innermost open transaction span, recording how it ended. */
    private static function closeTransaction(object $event, string $outcome): void
    {
        try {
            $span = array_pop(self::$transactionSpans);
            if (!$span instanceof Span) {
                return;
            }
            if (!$span->isVoid()) {
                $span->add('db.transaction.outcome', $outcome);
                // A rollback is not an error — application code rolls back on
                // purpose — but it is the thing a reader scanning a trace is
                // looking for, so it is marked as a status rather than left to be
                // found by reading attributes.
                if ($outcome === 'rollback') {
                    $span->markError();
                }
            }
            $span->finish();
        } catch (Throwable) {
        }
    }

    /**
     * Close transaction spans still open at request end.
     *
     * A transaction abandoned by a fatal error never raises a commit or a
     * rollback, and an unfinished span is never written at all — so without this
     * the queries beneath it would vanish along with it, which is the opposite of
     * what a trace of a failing request should do.
     */
    public static function closeDanglingTransactions(): void
    {
        while (self::$transactionSpans !== []) {
            $span = array_pop(self::$transactionSpans);
            if (!$span instanceof Span) {
                continue;
            }
            try {
                if (!$span->isVoid()) {
                    $span->add('db.transaction.outcome', 'abandoned');
                    $span->markError();
                }
                $span->finish();
            } catch (Throwable) {
            }
        }
    }

    /** The nesting depth this event was raised at; 1 is the outermost transaction. */
    private static function transactionLevel(object $event): int
    {
        try {
            $connection = $event->connection ?? null;
            if (is_object($connection) && method_exists($connection, 'transactionLevel')) {
                $level = $connection->transactionLevel();
                if (is_int($level) && $level > 0) {
                    return $level;
                }
            }
        } catch (Throwable) {
        }

        return 1;
    }

    private static function traceCacheRead(object $event): void
    {
        $store = isset($event->storeName) && is_string($event->storeName) && $event->storeName !== ''
            ? $event->storeName
            : 'cache';
        $span = SpanManager::open($store . ' GET');
        $commandMs = self::claimCacheCommandMs();
        if ($commandMs !== null) {
            $span->backdateStart($commandMs);
        }
        $key = (string) ($event->key ?? '');
        if (!$span->isVoid()) {
            $span->add('span.kind', 'client');
            $span->add('cache.system', 'redis');
            $span->add('cache.store', $store);
            $span->add('db.operation', 'GET');
            self::addRedisMetadata($span, $store);
            [$file, $line] = self::callSite();
            if ($file !== null) {
                $span->add('code.filepath', $file);
                $span->add('code.lineno', (string) $line);
            }
            $span->add('cache_key', $key);
            $hit = $event instanceof CacheHit;
            CacheCapture::stamp($span, $hit, $hit ? ($event->value ?? null) : null);
        }
        $span->finish();
        $hit = $event instanceof CacheHit ? '1' : '0';
        NativeExtension::recordDstEffect('cache_read', array_merge([
            'key' => self::clip($key, 256),
            'hit' => $hit,
            'store' => $store,
        ], self::dstCallSitePayload($span)));
    }

    private static function traceCacheWrite(object $event): void
    {
        $store = isset($event->storeName) && is_string($event->storeName) && $event->storeName !== ''
            ? $event->storeName
            : 'cache';
        $key = (string) ($event->key ?? '');
        $span = SpanManager::open($store . ' SET');
        $commandMs = self::claimCacheCommandMs();
        if ($commandMs !== null) {
            $span->backdateStart($commandMs);
        }
        if (!$span->isVoid()) {
            $span->add('span.kind', 'client');
            $span->add('cache.system', 'redis');
            $span->add('cache.store', $store);
            $span->add('db.operation', 'SET');
            self::addRedisMetadata($span, $store);
            [$file, $line] = self::callSite();
            if ($file !== null) {
                $span->add('code.filepath', $file);
                $span->add('code.lineno', (string) $line);
            }
            $span->add('cache_key', $key);
            if (isset($event->seconds) && is_numeric($event->seconds)) {
                $span->add('cache.ttl', (string) $event->seconds);
            }
        }
        $span->finish();
        NativeExtension::recordDstEffect('cache_write', array_merge([
            'key' => self::clip($key, 256),
            'store' => $store,
        ], self::dstCallSitePayload($span)));
    }

    private static function traceCacheForget(object $event): void
    {
        $store = isset($event->storeName) && is_string($event->storeName) && $event->storeName !== ''
            ? $event->storeName
            : 'cache';
        $key = (string) ($event->key ?? '');
        $span = SpanManager::open($store . ' DEL');
        $commandMs = self::claimCacheCommandMs();
        if ($commandMs !== null) {
            $span->backdateStart($commandMs);
        }
        if (!$span->isVoid()) {
            $span->add('span.kind', 'client');
            $span->add('cache.system', 'redis');
            $span->add('cache.store', $store);
            $span->add('db.operation', 'DEL');
            self::addRedisMetadata($span, $store);
            [$file, $line] = self::callSite();
            if ($file !== null) {
                $span->add('code.filepath', $file);
                $span->add('code.lineno', (string) $line);
            }
            $span->add('cache_key', $key);
        }
        $span->finish();
    }

    private static function traceOutboundHttp(): void
    {
        Http::globalMiddleware(static function (callable $handler): callable {
            return static function ($request, array $options) use ($handler) {
                $host = method_exists($request, 'getUri') ? (string) $request->getUri()->getHost() : '';
                $method = method_exists($request, 'getMethod') ? (string) $request->getMethod() : '';
                $url = method_exists($request, 'getUri') ? (string) $request->getUri() : '';
                $span = SpanManager::open('HTTP ' . ($host !== '' ? $host : 'unknown'));
                if (!$span->isVoid() && method_exists($request, 'withHeader')) {
                    $traceparent = NativeExtension::childTraceparent()
                        ?? ('00-' . $span->traceId . '-' . $span->id . '-01');
                    $request = $request->withHeader('traceparent', $traceparent);
                }
                if (!$span->isVoid()) {
                    self::attachOutboundRequest($span, $request, $method);
                }
                NativeExtension::recordDstEffect('http_request', array_merge([
                    'method' => $method !== '' ? $method : 'GET',
                    'url' => self::clip($url, 2048),
                ], self::dstCallSitePayload($span)));

                return $handler($request, $options)->then(
                    static function ($response) use ($span) {
                        $status = '0';
                        if (!$span->isVoid()) {
                            self::attachOutboundResponse($span, $response);
                        }
                        if (is_object($response) && method_exists($response, 'getStatusCode')) {
                            $status = (string) $response->getStatusCode();
                        }
                        NativeExtension::recordDstEffect('http_response', array_merge([
                            'status' => $status,
                        ], self::dstCallSitePayload($span)));
                        $span->finish();
                        return $response;
                    },
                    static function ($reason) use ($span) {
                        NativeExtension::recordDstEffect('http_response', array_merge([
                            'status' => '0',
                            'error' => '1',
                        ], self::dstCallSitePayload($span)));
                        $span->finish();
                        throw $reason;
                    },
                );
            };
        });
    }

    private static function clip(string $value, int $max): string
    {
        if (strlen($value) <= $max) {
            return $value;
        }

        return substr($value, 0, $max);
    }

    private static function traceHttpRequestSending(object $event): void
    {
        $request = $event->request ?? null;
        if (!is_object($request)) {
            return;
        }
        $method = method_exists($request, 'method') ? (string) $request->method() : '';
        $url = method_exists($request, 'url') ? (string) $request->url() : '';
        $host = '';
        try {
            $host = (string) (parse_url($url, PHP_URL_HOST) ?? '');
        } catch (Throwable) {
        }
        $span = SpanManager::open('HTTP ' . ($host !== '' ? $host : 'unknown'));
        if (!$span->isVoid()) {
            self::attachOutboundRequest($span, $request, $method !== '' ? $method : 'GET');
            // Stash for ResponseReceived pairing within this request.
            $span->add('chronos.http.pending', '1');
        }
        // Finish happens on ResponseReceived; if that never fires, request_end still flushes.
        self::$pendingHttpSpans[] = $span;
        NativeExtension::recordDstEffect('http_request', array_merge([
            'method' => $method !== '' ? $method : 'GET',
            'url' => self::clip($url, 2048),
        ], self::dstCallSitePayload($span)));
    }

    /** @var list<object> */
    private static array $pendingHttpSpans = [];

    private static function traceHttpResponseReceived(object $event): void
    {
        $response = $event->response ?? null;
        $status = '0';
        if (is_object($response) && method_exists($response, 'status')) {
            $status = (string) $response->status();
        } elseif (is_object($response) && method_exists($response, 'getStatusCode')) {
            $status = (string) $response->getStatusCode();
        }
        $span = array_pop(self::$pendingHttpSpans);
        if (is_object($span) && method_exists($span, 'isVoid') && !$span->isVoid()) {
            if (is_object($response)) {
                self::attachOutboundResponse($span, $response);
            }
            $span->finish();
        }
        $siteSpan = is_object($span) && method_exists($span, 'isVoid') ? $span : null;
        NativeExtension::recordDstEffect('http_response', array_merge([
            'status' => $status,
        ], self::dstCallSitePayload($siteSpan instanceof Span ? $siteSpan : null)));
    }

    private static function attachOutboundRequest(Span $span, object $request, string $method): void
    {
        try {
            $span->add('span.kind', 'client');
            $span->add('http.method', $method);
            $span->add('http.request.method', $method);
            $url = '';
            if (method_exists($request, 'url')) {
                $url = (string) $request->url();
            } elseif (method_exists($request, 'getUri')) {
                $url = (string) $request->getUri();
            }
            if ($url !== '') {
                $span->add('http.url', $url);
                $span->add('url.full', $url);
                $host = (string) (parse_url($url, PHP_URL_HOST) ?? '');
                if ($host !== '') {
                    $span->add('server.address', $host);
                }
            }
        } catch (Throwable) {
        }
    }

    private static function attachOutboundResponse(Span $span, object $response): void
    {
        try {
            $status = null;
            if (method_exists($response, 'status')) {
                $status = (string) $response->status();
            } elseif (method_exists($response, 'getStatusCode')) {
                $status = (string) $response->getStatusCode();
            }
            if ($status !== null) {
                $span->add('http.status_code', $status);
                $span->add('http.response.status_code', $status);
            }
        } catch (Throwable) {
        }
    }

    private static function recordLog(object $event): void
    {
        try {
            $severity = Severity::fromPsr3(is_string($event->level ?? null) ? $event->level : '');
            $context = is_array($event->context ?? null) ? $event->context : [];
            $attributes = [
                'log.context.count' => (string) count($context),
                'log.context.keys' => Span::boundedParametersJson(array_keys($context)),
            ];
            $body = is_string($event->message ?? null) ? $event->message : '';
            NativeExtension::captureLog($severity['text'], $severity['number'], $body, $attributes);
        } catch (Throwable) {
        }
    }

    /**
     * The PDO handle behind a query event, for the EXPLAIN. Laravel's connection
     * has already executed the statement by the time DB::listen fires, so this
     * never opens a connection that was not open anyway.
     */
    private static function pdo(object $query): ?\PDO
    {
        try {
            $connection = $query->connection ?? null;
            if (!is_object($connection) || !method_exists($connection, 'getPdo')) {
                return null;
            }
            $handle = $connection->getPdo();

            return $handle instanceof \PDO ? $handle : null;
        } catch (Throwable) {
            return null;
        }
    }

    private static function addConnectionMetadata(object $span, object $query): void
    {
        try {
            $connection = $query->connection ?? null;
            if (!is_object($connection) || !method_exists($connection, 'getConfig')) {
                return;
            }
            $map = ['db.name' => 'database', 'db.host' => 'host', 'db.user' => 'username', 'db.driver' => 'driver'];
            foreach ($map as $attr => $key) {
                $value = $connection->getConfig($key);
                if (is_scalar($value) && (string) $value !== '') {
                    $span->add($attr, (string) $value);
                    if ($attr === 'db.host') {
                        $span->add('server.address', (string) $value);
                    }
                }
            }
        } catch (Throwable) {
        }
    }

    private static function addRedisMetadata(object $span, string $store): void
    {
        try {
            if (!function_exists('config')) {
                return;
            }
            $connectionName = config("cache.stores.{$store}.connection") ?? 'cache';
            $prefix = "database.redis.{$connectionName}";
            foreach (['cache.host' => 'host', 'cache.db' => 'database'] as $attr => $key) {
                $value = config("{$prefix}.{$key}");
                if (is_scalar($value) && (string) $value !== '') {
                    $span->add($attr, (string) $value);
                    if ($attr === 'cache.host') {
                        $span->add('server.address', (string) $value);
                    }
                }
            }
        } catch (Throwable) {
        }
    }

    /**
     * Call-site + correlation attributes for DST effect payloads so the
     * investigation UI can open historical source and jump to the matching span
     * without joining indexes by hand.
     *
     * - `file` / `line` / `function` — first-party call site
     * - `commit` — full git SHA when CI exposed one (worktree-safe history)
     * - `span_id` — the APM span opened for this effect (same request / trace)
     *
     * @return array{file?: string, line?: string, function?: string, commit?: string, span_id?: string}
     */
    private static function dstCallSitePayload(?Span $span = null): array
    {
        [$file, $line, $function] = self::callSite();
        $payload = [];
        if (is_string($file) && $file !== '') {
            $payload['file'] = self::clip($file, 512);
            if ($line > 0) {
                $payload['line'] = (string) $line;
            }
        }
        if (is_string($function) && $function !== '') {
            $payload['function'] = self::clip($function, 256);
        }
        $commit = AppVersion::commitSha();
        if ($commit !== null) {
            $payload['commit'] = self::clip($commit, 64);
        }
        $linked = $span;
        if ($linked === null || $linked->isVoid()) {
            $linked = SpanManager::active();
        }
        if ($linked !== null && !$linked->isVoid() && $linked->id !== '') {
            $payload['span_id'] = $linked->id;
        }
        return $payload;
    }

    /** @return array{0: string|null, 1: int, 2: string|null} */
    /**
     * The first application frame, shared with the activity catalogs so a span's
     * `code.*` attributes and a catalog entry's agree on what "the call site"
     * means.
     *
     * @return array{0: ?string, 1: int, 2: ?string}
     */
    private static function callSite(): array
    {
        return CallSite::firstApplicationFrame();
    }

    private static function sqlVerb(string $sql): string
    {
        $trimmed = ltrim($sql);
        $verb = strtoupper(substr($trimmed, 0, (int) strcspn($trimmed, " \t\n\r")));
        return $verb === '' ? 'QUERY' : $verb;
    }
}
