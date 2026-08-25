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
        if (!NativeExtension::enabled()) {
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
        if (!NativeExtension::active()) {
            // The collector declined this request (no identity envelope, or a CLI
            // context without CHRONOS_PHP_CLI_ENABLED) — everything below would be
            // FFI calls into no-ops.
            return $next($request);
        }
        // Language/framework/release identity for the `app.*` span attributes.
        // Resolved here rather than in the .so: only userland can read the
        // framework's version constant and the app's configured release.
        NativeExtension::setAppMetadata(
            'laravel',
            \class_exists(\Illuminate\Foundation\Application::class) ? app()->version() : '',
            (string) config('app.version', ''),
        );
        // RichTelemetryHooks (installed by the service provider whenever the
        // extension is loaded) owns SQL and cache spans with connection identity
        // — the native PDO/Redis fallbacks stand down. Per request: the native
        // flags reset at every requestStart.
        NativeExtension::suppressNative('sql');
        NativeExtension::suppressNative('cache');

        // The middleware stack below this point is the application's own work; every
        // millisecond before it was framework bootstrap. The Timeline tab splits the
        // request on exactly this boundary.
        NativeExtension::markPhase('dispatch');

        try {
            $response = $next($request);

            $resolvedRoute = $this->resolveRoute($request);
            $statusCode = is_object($response) && method_exists($response, 'getStatusCode')
                ? (int) $response->getStatusCode()
                : 0;

            // Laravel renders most exceptions into an error response instead of letting
            // them escape to middleware, attaching the original throwable to it — this
            // is the path real application errors actually take.
            $rendered = is_object($response) && isset($response->exception) && $response->exception instanceof Throwable
                ? $response->exception
                : null;

            // Rendered into a response by the framework's exception handler, so
            // handled=true; the catch branch below is the unhandled path.
            NativeExtension::markPhase('send');
            $this->captureResponse($response);
            $this->hydrateRoot($request);

            NativeExtension::requestEnd($statusCode, $resolvedRoute ?? $routePattern, $rendered, true);

            $this->injectDownstream($response);

            return $response;
        } catch (Throwable $e) {
            $this->hydrateRoot($request);
            NativeExtension::requestEnd(500, $routePattern, $e, false);
            throw $e;
        }
    }

    /**
     * Hand the rendered body and content type to the collector.
     *
     * Laravel returns a Symfony Response for HTTP routes, so getContent() is the
     * whole body — except for a StreamedResponse or a file download, where it is
     * false and correctly skipped. A JsonResponse arrives here already encoded,
     * which is exactly what the Response tab wants to pretty-print.
     */
    private function captureResponse(mixed $response): void
    {
        try {
            // getContent() copies the whole body; on an unsampled request the
            // collector would drop it anyway, so don't pay for the copy.
            if (!NativeExtension::httpCapturing()) {
                return;
            }
            if (!is_object($response) || !method_exists($response, 'getContent')) {
                return;
            }
            $body = $response->getContent();
            $body = is_string($body) ? $body : '';
            $hasBag = isset($response->headers) && method_exists($response->headers, 'get');
            $contentType = $hasBag ? (string) $response->headers->get('Content-Type', '') : '';
            $headers = $hasBag && method_exists($response->headers, 'all')
                ? NativeExtension::flattenHeaders($response->headers->all())
                : [];
            NativeExtension::setResponseBody($body, $contentType, $headers);
        } catch (Throwable) {
            // Capture is never allowed to break the response it is observing.
        }
    }

    private function hydrateRoot(Request $request): void
    {
        try {
            $identity = $this->requestIdentity($request);
            $action = $identity['http.route.action'] ?? '';
            if ($action !== '' && function_exists('chronos_profile_tag')) {
                try {
                    \chronos_profile_tag('action', $action);
                } catch (Throwable) {
                }
            }
            RequestFacts::flush($identity);
        } catch (Throwable) {
            RequestFacts::reset();
        }
    }

    /** @return array<string, string> */
    private function requestIdentity(Request $request): array
    {
        $routeName = '';
        $action = '';
        $middleware = [];
        try {
            $route = method_exists($request, 'route') ? $request->route() : null;
            if (is_object($route)) {
                $name = method_exists($route, 'getName') ? $route->getName() : null;
                $routeName = is_string($name) ? $name : '';
                $actionName = method_exists($route, 'getActionName') ? $route->getActionName() : null;
                $action = is_string($actionName) ? $actionName : '';
                if (method_exists($route, 'gatherMiddleware')) {
                    foreach ((array) $route->gatherMiddleware() as $entry) {
                        if (is_string($entry) && $entry !== '') {
                            $middleware[] = $entry;
                        } elseif ($entry instanceof Closure) {
                            $middleware[] = 'Closure';
                        }
                    }
                }
            }
        } catch (Throwable) {
        }
        $userId = '';
        $guard = '';
        try {
            if (function_exists('auth')) {
                $auth = auth();
                if (is_object($auth) && method_exists($auth, 'getDefaultDriver')) {
                    $guard = (string) $auth->getDefaultDriver();
                }
                if (is_object($auth) && method_exists($auth, 'id')) {
                    $id = $auth->id();
                    if (is_scalar($id) && (string) $id !== '') {
                        $userId = (string) $id;
                    }
                }
            }
        } catch (Throwable) {
        }
        $peak = 0;
        try {
            $peak = memory_get_peak_usage(true);
        } catch (Throwable) {
        }

        return RequestFacts::identity($routeName, $action, $middleware, $userId, $guard, $peak);
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
