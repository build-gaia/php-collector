<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Psr18;

use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Propagation;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Throwable;

/**
 * Decorates any PSR-18 HTTP client so it gets a client span and trace propagation
 * without a framework-specific bridge. Guzzle already has TraceparentMiddleware
 * plus the Laravel Http-facade listeners in RichTelemetryHooks; this is the same
 * idea for the many other PSR-18 implementations an application might hold
 * directly (Symfony HttpClient's Psr18Client, php-http/curl-client, a test
 * double, …) — anything typed against Psr\Http\Client\ClientInterface rather
 * than a concrete class.
 *
 * psr/http-client and psr/http-message are NOT dependencies of this package
 * (the zero-runtime-dependency constraint in composer.json is absolute), so —
 * same technique as ChronosHandler/ChronosLogger/ChronosTraceparentStamp next
 * door — the whole class declaration sits behind an `interface_exists()` guard
 * rather than a hard `use`+`implements`: PHP only resolves an `implements`
 * target when the class declaration statement EXECUTES, so a false condition
 * here skips declaring the class entirely and this file loads cleanly whether
 * or not psr/http-client is on the app's classmap. This also protects against
 * opcache preloading, which resolves every class in every file up front
 * regardless of whether the application ever constructs one — plain lazy
 * PSR-4 autoloading is not a strong enough guarantee on its own.
 *
 * Fail-open throughout: a broken span must never turn a working HTTP call into
 * a broken one, so every telemetry step is wrapped and swallowed. The one
 * exception is the inner call itself and its exception, which are never
 * caught — a ClientExceptionInterface is annotated onto the span and rethrown
 * unchanged, exactly as the caller's `catch` expects.
 */
if (interface_exists(\Psr\Http\Client\ClientInterface::class)) {
    final class ChronosClientDecorator implements \Psr\Http\Client\ClientInterface
    {
        public function __construct(
            private readonly \Psr\Http\Client\ClientInterface $inner,
        ) {
        }

        public function sendRequest(\Psr\Http\Message\RequestInterface $request): \Psr\Http\Message\ResponseInterface
        {
            $request = $this->withTraceparent($request);
            $span = $this->beginSpan($request);

            try {
                $response = $this->inner->sendRequest($request);
                $this->attachResponse($span, $response);

                return $response;
            } catch (\Psr\Http\Client\ClientExceptionInterface $exception) {
                $this->attachException($span, $exception);
                throw $exception;
            } finally {
                $this->finish($span);
            }
        }

        /**
         * Inject a child traceparent so the callee joins this request's trace.
         * Skipped when the caller already set one — an application-supplied
         * traceparent (e.g. manual propagation, a replayed header) is never
         * overwritten, mirroring TraceparentMiddleware's own rule.
         */
        private function withTraceparent(\Psr\Http\Message\RequestInterface $request): \Psr\Http\Message\RequestInterface
        {
            try {
                if (!$request->hasHeader('traceparent')) {
                    $traceparent = NativeExtension::childTraceparent();
                    if ($traceparent !== null) {
                        $request = $request->withHeader('traceparent', $traceparent);
                    }
                }
                // W3C Trace Context: forwarding traceparent obliges forwarding
                // tracestate too; baggage follows the same forward-as-is rule.
                // Application-set headers are never overwritten, same as above.
                foreach (Propagation::contextHeaders() as $name => $value) {
                    if (!$request->hasHeader($name)) {
                        $request = $request->withHeader($name, $value);
                    }
                }
            } catch (Throwable) {
                // A propagation failure must never block the request from being sent.
            }

            return $request;
        }

        private function beginSpan(\Psr\Http\Message\RequestInterface $request): ?Span
        {
            try {
                $span = SpanManager::open('HTTP '.$request->getMethod());
                if ($span->isVoid()) {
                    return $span;
                }
                $span->add('span.kind', 'client');
                $method = $request->getMethod();
                $span->add('http.request.method', $method);
                $uri = $request->getUri();
                $url = (string) $uri;
                if ($url !== '') {
                    $span->add('url.full', $url);
                }
                $host = $uri->getHost();
                if ($host !== '') {
                    $span->add('server.address', $host);
                }
                $port = $this->resolvePort($uri->getScheme(), $uri->getPort());
                if ($port !== null) {
                    $span->add('server.port', (string) $port);
                }

                return $span;
            } catch (Throwable) {
                return null;
            }
        }

        /**
         * PSR-7's getPort() is null whenever the URI carries the scheme's default
         * port (http/80, https/443) — the standard says implementations MAY omit
         * it in that case, and most do. server.port is meant to name the port the
         * connection actually used, so the scheme default is filled in rather than
         * leaving the attribute off entirely for the common case.
         */
        private function resolvePort(string $scheme, ?int $explicit): ?int
        {
            if ($explicit !== null) {
                return $explicit;
            }

            return match (strtolower($scheme)) {
                'http' => 80,
                'https' => 443,
                default => null,
            };
        }

        private function attachResponse(?Span $span, \Psr\Http\Message\ResponseInterface $response): void
        {
            if (!$span instanceof Span) {
                return;
            }
            try {
                $span->add('http.response.status_code', (string) $response->getStatusCode());
            } catch (Throwable) {
            }
        }

        private function attachException(?Span $span, \Psr\Http\Client\ClientExceptionInterface $exception): void
        {
            if (!$span instanceof Span) {
                return;
            }
            try {
                // ClientExceptionInterface is a marker interface with no fields of its
                // own, so the concrete exception class name IS the error identity a
                // trace can group by — the same reading NativeExtension::requestEnd
                // gives get_class($exception) for the request root.
                $span->add('error.type', get_class($exception));
                $span->recordException($exception, false);
            } catch (Throwable) {
            }
        }

        private function finish(?Span $span): void
        {
            if (!$span instanceof Span) {
                return;
            }
            try {
                $span->finish();
            } catch (Throwable) {
            }
        }
    }
}
