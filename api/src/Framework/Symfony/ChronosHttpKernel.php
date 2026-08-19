<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony;

use Chronos\Collector\Service\NativeExtension;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\HttpKernel\HttpKernelInterface;
use Throwable;

/**
 * Decorating HttpKernel that bridges Symfony's request lifecycle to the native
 * Rust collector. The .so owns all APM, profiling, log batching, DST, and metrics.
 */
final class ChronosHttpKernel implements HttpKernelInterface
{
    public function __construct(private readonly HttpKernelInterface $kernel)
    {
    }

    public function handle(
        Request $request,
        int $type = self::MAIN_REQUEST,
        bool $catch = true,
    ): Response {
        if ($type !== self::MAIN_REQUEST || !NativeExtension::loaded()) {
            return $this->kernel->handle($request, $type, $catch);
        }

        $hasHeaders = isset($request->headers) && method_exists($request->headers, 'get');
        $traceparent = $hasHeaders ? $request->headers->get('traceparent') : null;
        $tracestate = $hasHeaders ? $request->headers->get('tracestate') : null;
        $baggage = $hasHeaders ? $request->headers->get('baggage') : null;

        NativeExtension::requestStart(
            is_string($traceparent) ? $traceparent : null,
            is_string($tracestate) ? $tracestate : null,
            is_string($baggage) ? $baggage : null,
            null,
            null,
            $request->getMethod(),
            $request->getPathInfo(),
            'symfony',
        );

        try {
            $response = $this->kernel->handle($request, $type, $catch);

            $route = $this->resolveRoute($request);
            $statusCode = method_exists($response, 'getStatusCode') ? (int) $response->getStatusCode() : 0;

            NativeExtension::requestEnd($statusCode, $route ?? $request->getPathInfo());

            $this->injectDownstream($response);

            return $response;
        } catch (Throwable $e) {
            NativeExtension::requestEnd(500, $request->getPathInfo());
            throw $e;
        }
    }

    private function resolveRoute(Request $request): ?string
    {
        if (!isset($request->attributes) || !method_exists($request->attributes, 'get')) {
            return null;
        }
        $route = $request->attributes->get('_route');
        return is_string($route) && $route !== '' ? $route : null;
    }

    private function injectDownstream(Response $response): void
    {
        if (!isset($response->headers) || !method_exists($response->headers, 'set')) {
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
