<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Chronos\Collector\Service\TraceContext;
use Closure;
use Illuminate\Http\Request;
use Throwable;

/**
 * Laravel global middleware. Bridges the Laravel request lifecycle to the native
 * Rust collector. The .so owns all APM, profiling, log batching, DST, and metrics
 * collection; this middleware just passes context in and status out.
 */
final class RecordChronosRequest
{
    public function handle(Request $request, Closure $next): mixed
    {
        if (!NativeExtension::loaded()) {
            return $next($request);
        }

        $traceparent = $request->header('traceparent');
        $tracestate = $request->header('tracestate');
        $baggage = $request->header('baggage');
        $sessionId = $request->header('x-chronos-session-id');
        $dstDirective = $request->header('x-chronos-dst') ?? $request->cookie('chronos_dst');

        $routePattern = $request->path();
        $httpMethod = $request->method();
        $serviceName = config('app.name', 'laravel');

        NativeExtension::requestStart(
            is_string($traceparent) ? $traceparent : null,
            is_string($tracestate) ? $tracestate : null,
            is_string($baggage) ? $baggage : null,
            is_string($sessionId) ? $sessionId : null,
            is_string($dstDirective) ? $dstDirective : null,
            $httpMethod,
            $routePattern,
            $serviceName,
        );

        try {
            $response = $next($request);

            $resolvedRoute = $this->resolveRoute($request);
            $statusCode = is_object($response) && method_exists($response, 'getStatusCode')
                ? (int) $response->getStatusCode()
                : 0;

            NativeExtension::requestEnd($statusCode, $resolvedRoute ?? $routePattern);

            $this->injectDownstream($response);

            return $response;
        } catch (Throwable $e) {
            NativeExtension::requestEnd(500, $routePattern);
            throw $e;
        }
    }

    private function resolveRoute(Request $request): ?string
    {
        $route = method_exists($request, 'route') ? $request->route() : null;
        if ($route === null) {
            return null;
        }
        $uri = method_exists($route, 'uri') ? $route->uri() : null;
        if (is_string($uri) && $uri !== '') {
            return $uri;
        }
        $name = method_exists($route, 'getName') ? $route->getName() : null;
        return is_string($name) && $name !== '' ? $name : null;
    }

    private function injectDownstream(mixed $response): void
    {
        if (!is_object($response) || !isset($response->headers) || !method_exists($response->headers, 'set')) {
            return;
        }
        try {
            $traceparent = NativeExtension::traceparent();
            if ($traceparent !== null) {
                $response->headers->set('traceparent', $traceparent);
            }
        } catch (Throwable) {
        }
    }
}
