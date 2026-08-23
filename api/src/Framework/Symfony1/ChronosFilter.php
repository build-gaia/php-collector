<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony1;

use Chronos\Collector\Service\NativeExtension;
use Throwable;

/**
 * Symfony 1.x filter that bridges the request lifecycle to the native Rust
 * collector. The .so owns all APM, profiling, log batching, DST, and metrics.
 * This filter also attaches Doctrine span listeners for userland SQL spans.
 */
final class ChronosFilter extends \sfFilter
{
    public function execute($filterChain): void
    {
        if (!NativeExtension::loaded()) {
            $filterChain->execute();
            return;
        }

        $request = $this->context->getRequest();
        $hasHeader = method_exists($request, 'getHttpHeader');

        $traceparent = $hasHeader ? $request->getHttpHeader('traceparent') : null;
        $tracestate = $hasHeader ? $request->getHttpHeader('tracestate') : null;
        $baggage = $hasHeader ? $request->getHttpHeader('baggage') : null;
        $sessionId = $hasHeader ? $request->getHttpHeader('x-chronos-session-id') : null;
        $dstDirective = self::dstDirective($request);

        NativeExtension::requestStart(
            is_string($traceparent) ? $traceparent : null,
            is_string($tracestate) ? $tracestate : null,
            is_string($baggage) ? $baggage : null,
            is_string($sessionId) ? $sessionId : null,
            is_string($dstDirective) ? $dstDirective : null,
            $request->getMethod(),
            $request->getPathInfo(),
            // Empty service name: the native collector falls back to the
            // CHRONOS_PHP_APPLICATION identity, keeping the service map's node
            // names aligned with the application id.
            '',
        );

        // Framework identity for the `app.*` span attributes. Symfony 1 reports its
        // version through SYMFONY_VERSION when the core is loaded.
        NativeExtension::setAppMetadata(
            'symfony1',
            \defined('SYMFONY_VERSION') ? (string) \SYMFONY_VERSION : '',
        );

        self::attachDoctrineSpans();
        self::attachLogListener($this->context);

        // Everything up to here — sfContext creation, autoloading, config cache,
        // Doctrine connection setup — is the bootstrap phase the Timeline tab shows
        // first. The filter chain is where the application's own work begins.
        NativeExtension::markPhase('dispatch');

        try {
            $filterChain->execute();

            $route = self::resolveRoute($this->context);
            $statusCode = self::resolveStatusCode($this->context);

            // The action has run and the response is fully rendered but not yet sent:
            // the one moment the body exists in memory and can be captured exactly.
            NativeExtension::markPhase('send');
            self::captureResponse($this->context);

            NativeExtension::requestEnd($statusCode, $route ?? $request->getPathInfo(), null, true);
        } catch (Throwable $e) {
            // The response object still holds whatever was set before the throw —
            // usually headers only, since symfony renders its exception page after
            // this filter unwinds. Capturing it anyway is what makes an errored
            // request's Response tab exact rather than a headers_list() guess made
            // before the framework flushed anything.
            self::captureResponse($this->context);
            NativeExtension::requestEnd(500, $request->getPathInfo(), $e, false);
            throw $e;
        }
    }

    private static function dstDirective(object $request): ?string
    {
        try {
            if (method_exists($request, 'getHttpHeader')) {
                $header = $request->getHttpHeader('x-chronos-dst');
                if (is_string($header) && trim($header) !== '') {
                    return $header;
                }
            }
            if (method_exists($request, 'getCookie')) {
                $cookie = $request->getCookie('chronos_dst');
                if (is_string($cookie) && trim($cookie) !== '') {
                    return $cookie;
                }
            }
        } catch (Throwable) {
        }
        return null;
    }

    private static function attachLogListener(object $context): void
    {
        try {
            if (!method_exists($context, 'getEventDispatcher')) {
                return;
            }
            $dispatcher = $context->getEventDispatcher();
            if (!is_object($dispatcher) || !method_exists($dispatcher, 'connect')) {
                return;
            }
            $listener = new ChronosLogListener();
            $dispatcher->connect('application.log', [$listener, 'onLog']);
            $dispatcher->connect('application.throw_exception', [$listener, 'onException']);
        } catch (Throwable) {
        }
    }

    private static function attachDoctrineSpans(): void
    {
        if (!class_exists('\Doctrine_Manager')) {
            return;
        }
        try {
            $listener = new DoctrineSpanListener();
            $manager = \Doctrine_Manager::getInstance();
            foreach ($manager as $connection) {
                if (method_exists($connection, 'addListener')) {
                    $connection->addListener($listener);
                }
            }
            if (method_exists($manager, 'addListener')) {
                $manager->addListener($listener);
            }
            // The Doctrine listener owns SQL spans (host/db/bound params) — the
            // native PDO fallback stands down for this request.
            NativeExtension::suppressNative('sql');
        } catch (Throwable) {
        }
    }

    /**
     * Hand the rendered response body and its content type to the collector.
     *
     * symfony1 holds the whole body in sfWebResponse::getContent() right up to
     * sendContent(), so this is exact and costs nothing. A streaming response
     * reports null content and is correctly skipped rather than forced into memory.
     */
    private static function captureResponse(object $context): void
    {
        try {
            $response = method_exists($context, 'getResponse') ? $context->getResponse() : null;
            if (!is_object($response) || !method_exists($response, 'getContent')) {
                return;
            }
            $contentType = method_exists($response, 'getContentType')
                ? (string) $response->getContentType()
                : '';
            // sfWebResponse holds its headers until sendHttpHeaders(), which runs
            // after this filter returns — so the collector's own headers_list()
            // read would find nothing. Supply them here.
            $headers = method_exists($response, 'getHttpHeaders')
                ? NativeExtension::flattenHeaders((array) $response->getHttpHeaders())
                : [];
            NativeExtension::setResponseBody($response->getContent(), $contentType, $headers);
        } catch (Throwable) {
            // Capture is never allowed to break the response it is observing.
        }
    }

    private static function resolveRoute(object $context): ?string
    {
        $module = method_exists($context, 'getModuleName') ? $context->getModuleName() : null;
        $action = method_exists($context, 'getActionName') ? $context->getActionName() : null;
        if (is_string($module) && is_string($action) && $module !== '' && $action !== '') {
            return $module . '/' . $action;
        }
        return null;
    }

    private static function resolveStatusCode(object $context): int
    {
        $response = method_exists($context, 'getResponse') ? $context->getResponse() : null;
        if (is_object($response) && method_exists($response, 'getStatusCode')) {
            return (int) $response->getStatusCode();
        }
        return 0;
    }
}
