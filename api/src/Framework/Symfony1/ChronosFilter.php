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
            'symfony1',
        );

        self::attachDoctrineSpans();
        self::attachLogListener($this->context);

        try {
            $filterChain->execute();

            $route = self::resolveRoute($this->context);
            $statusCode = self::resolveStatusCode($this->context);

            NativeExtension::requestEnd($statusCode, $route ?? $request->getPathInfo());
        } catch (Throwable $e) {
            NativeExtension::requestEnd(500, $request->getPathInfo());
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
        } catch (Throwable) {
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
