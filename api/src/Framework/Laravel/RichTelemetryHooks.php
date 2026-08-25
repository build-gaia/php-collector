<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\QueryPlan;
use Chronos\Collector\Service\Severity;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Illuminate\Cache\Events\CacheHit;
use Illuminate\Cache\Events\CacheMissed;
use Illuminate\Cache\Events\KeyForgotten;
use Illuminate\Cache\Events\KeyWritten;
use Illuminate\Http\Client\Events\RequestSending;
use Illuminate\Http\Client\Events\ResponseReceived;
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
        NativeExtension::recordDstEffect('database_query', [
            'statement' => self::clip($sql, 4096),
        ]);
        NativeExtension::recordDstEffect('database_result', [
            'statement' => self::clip($sql, 4096),
            'duration_ms' => (string) ($query->time ?? 0),
        ]);
    }

    private static function traceCacheRead(object $event): void
    {
        $store = isset($event->storeName) && is_string($event->storeName) && $event->storeName !== ''
            ? $event->storeName
            : 'cache';
        $span = SpanManager::open($store . ' GET');
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
            $span->add('cache.hit', $event instanceof CacheHit ? 'true' : 'false');
        }
        $span->finish();
        $hit = $event instanceof CacheHit ? '1' : '0';
        NativeExtension::recordDstEffect('cache_read', [
            'key' => self::clip($key, 256),
            'hit' => $hit,
            'store' => $store,
        ]);
    }

    private static function traceCacheWrite(object $event): void
    {
        $store = isset($event->storeName) && is_string($event->storeName) && $event->storeName !== ''
            ? $event->storeName
            : 'cache';
        $key = (string) ($event->key ?? '');
        $span = SpanManager::open($store . ' SET');
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
        NativeExtension::recordDstEffect('cache_write', [
            'key' => self::clip($key, 256),
            'store' => $store,
        ]);
    }

    private static function traceCacheForget(object $event): void
    {
        $store = isset($event->storeName) && is_string($event->storeName) && $event->storeName !== ''
            ? $event->storeName
            : 'cache';
        $key = (string) ($event->key ?? '');
        $span = SpanManager::open($store . ' DEL');
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
                NativeExtension::recordDstEffect('http_request', [
                    'method' => $method !== '' ? $method : 'GET',
                    'url' => self::clip($url, 2048),
                ]);

                return $handler($request, $options)->then(
                    static function ($response) use ($span) {
                        $status = '0';
                        if (!$span->isVoid()) {
                            self::attachOutboundResponse($span, $response);
                        }
                        if (is_object($response) && method_exists($response, 'getStatusCode')) {
                            $status = (string) $response->getStatusCode();
                        }
                        NativeExtension::recordDstEffect('http_response', [
                            'status' => $status,
                        ]);
                        $span->finish();
                        return $response;
                    },
                    static function ($reason) use ($span) {
                        NativeExtension::recordDstEffect('http_response', [
                            'status' => '0',
                            'error' => '1',
                        ]);
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
        NativeExtension::recordDstEffect('http_request', [
            'method' => $method !== '' ? $method : 'GET',
            'url' => self::clip($url, 2048),
        ]);
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
        NativeExtension::recordDstEffect('http_response', [
            'status' => $status,
        ]);
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

    /** @return array{0: string|null, 1: int, 2: string|null} */
    private static function callSite(): array
    {
        try {
            $frames = debug_backtrace(DEBUG_BACKTRACE_IGNORE_ARGS, 40);
            foreach ($frames as $index => $frame) {
                $file = $frame['file'] ?? null;
                if (is_string($file) && $file !== '' && !str_contains($file, '/vendor/')) {
                    return [$file, (int) ($frame['line'] ?? 0), self::frameFunction($frames[$index + 1] ?? null)];
                }
            }
        } catch (Throwable) {
        }
        return [null, 0, null];
    }

    /** @param array<string, mixed>|null $frame */
    private static function frameFunction(?array $frame): ?string
    {
        if ($frame === null) {
            return null;
        }
        $function = is_string($frame['function'] ?? null) ? $frame['function'] : '';
        if ($function === '') {
            return null;
        }
        $class = is_string($frame['class'] ?? null) ? $frame['class'] : '';
        return $class === '' ? $function : $class . '::' . $function;
    }

    private static function sqlVerb(string $sql): string
    {
        $trimmed = ltrim($sql);
        $verb = strtoupper(substr($trimmed, 0, (int) strcspn($trimmed, " \t\n\r")));
        return $verb === '' ? 'QUERY' : $verb;
    }
}
