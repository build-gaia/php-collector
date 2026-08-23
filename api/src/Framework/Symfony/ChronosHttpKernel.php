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
        if ($type !== self::MAIN_REQUEST || !NativeExtension::enabled()) {
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
            // Empty service name → native falls back to CHRONOS_PHP_APPLICATION.
            '',
        );
        if (!NativeExtension::active()) {
            // The collector declined this request — everything below would be FFI
            // calls into no-ops.
            return $this->kernel->handle($request, $type, $catch);
        }

        // Framework identity for the `app.*` span attributes. Symfony exposes its
        // version as a Kernel constant; the app's own release stays config-driven.
        NativeExtension::setAppMetadata(
            'symfony',
            \defined('Symfony\\Component\\HttpKernel\\Kernel::VERSION')
                ? \Symfony\Component\HttpKernel\Kernel::VERSION
                : '',
        );

        // Container boot, bundle registration and kernel warmup are behind us; the
        // kernel call below is the application's own work. Marks the boundary the
        // Timeline tab draws between "bootstrap" and "dispatch".
        NativeExtension::markPhase('dispatch');

        try {
            $response = $this->kernel->handle($request, $type, $catch);

            $route = $this->resolveRoute($request);
            $statusCode = method_exists($response, 'getStatusCode') ? (int) $response->getStatusCode() : 0;

            NativeExtension::markPhase('send');
            $this->captureResponse($response);

            NativeExtension::requestEnd($statusCode, $route ?? $request->getPathInfo(), null, true);

            $this->injectDownstream($response);

            return $response;
        } catch (Throwable $e) {
            NativeExtension::requestEnd(500, $request->getPathInfo(), $e, false);
            throw $e;
        }
    }

    /**
     * Hand the rendered body and content type to the collector, one moment before
     * Symfony sends it.
     *
     * A StreamedResponse or BinaryFileResponse has no in-memory body — getContent()
     * returns false — and is skipped rather than forced to materialise, which would
     * turn an observation into a behaviour change (and, for a file download, an OOM).
     */
    private function captureResponse(Response $response): void
    {
        try {
            // getContent() copies the whole body; on an unsampled request the
            // collector would drop it anyway, so don't pay for the copy.
            if (!NativeExtension::httpCapturing()) {
                return;
            }
            if (!method_exists($response, 'getContent')) {
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
