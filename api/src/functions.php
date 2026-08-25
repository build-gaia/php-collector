<?php

declare(strict_types=1);

/**
 * Global instrumentation API, loaded via composer.json `autoload.files` rather than PSR-4 because
 * these are free functions in the `Chronos\` namespace, mirroring ddtrace's `\DDTrace\trace_method`.
 * An application declares its span decorations in a SEPARATE manifest file (exactly like
 * deepwell/datadog/instrumentation.php uses \DDTrace\trace_method) and either require()s it or points
 * CHRONOS_PHP_INSTRUMENTATION_MANIFEST at it.
 *
 * Every function is guarded with function_exists(): once the native extension defines the same
 * symbols, the extension's implementations win and this file becomes a no-op, so an application that
 * ships the manifest keeps working whether or not the .so is installed — no redeclare fatal.
 */

namespace Chronos;

use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\SpanDecorations;

if (!function_exists('Chronos\\trace_method')) {
    /**
     * Register a span decoration for $class::$method (or a plain function when $class is '').
     *
     * Two things happen, matching the two capture paths:
     *   1. The decorator (if any) lands in the SpanDecorations registry, so the userland
     *      call-through path (SpanManager::callThrough) can invoke it with the child span.
     *   2. The qualified name is registered into the NATIVE extension's span allowlist
     *      (chronos_trace_function), so the Zend observer emits a span for every call —
     *      transparently, with no app-code change — tagged chronos.instrumented=manifest.
     *
     * v1 LIMITATION: the native observer cannot invoke PHP decorator callbacks from inside
     * its span capture — a natively-intercepted call yields a span with timing + name only.
     * Decorator-added attributes (user.id, order.* …) appear only when the call is routed
     * through SpanManager::callThrough. Do not expect decorator attributes on native spans.
     *
     * Registration itself never invokes anything and never throws.
     *
     * @param ?callable $decorator function(\Chronos\Collector\Service\Span $span, array $arguments): void
     */
    function trace_method(string $class, string $method, ?callable $decorator = null): void
    {
        try {
            if ($decorator !== null && $class !== '') {
                SpanDecorations::register($class, $method, $decorator);
            }
            $class = ltrim(trim($class), '\\');
            $method = trim($method);
            if ($method === '') {
                return;
            }
            NativeExtension::traceFunction($class === '' ? $method : $class.'::'.$method);
        } catch (\Throwable) {
            // Instrumentation registration must never take down the application.
        }
    }
}

if (!function_exists('Chronos\\load_instrumentation')) {
    /**
     * Load an application instrumentation manifest — a plain PHP file that calls Chronos\trace_method()
     * to declare its decorations. Fail-open: a missing or unreadable path, or a manifest that throws, is
     * swallowed so instrumentation configuration can never take down the application. require_once, so
     * the composer-autoload-time bootstrap and the per-request NativeExtension fallback can both try
     * without double-registering.
     */
    function load_instrumentation(string $path): void
    {
        try {
            if ($path !== '' && is_file($path) && is_readable($path)) {
                require_once $path;
            }
        } catch (\Throwable) {
        }
    }
}

// Auto-load the manifest named by CHRONOS_PHP_INSTRUMENTATION_MANIFEST, once per process. autoload.files
// includes this file exactly once, so this env-triggered load also happens exactly once at boot.
if (!function_exists('Chronos\\__chronos_bootstrap_instrumentation')) {
    function __chronos_bootstrap_instrumentation(): void
    {
        $manifest = getenv('CHRONOS_PHP_INSTRUMENTATION_MANIFEST');
        if (($manifest === false || $manifest === '') && isset($_SERVER['CHRONOS_PHP_INSTRUMENTATION_MANIFEST'])) {
            $manifest = (string) $_SERVER['CHRONOS_PHP_INSTRUMENTATION_MANIFEST'];
        }
        if (is_string($manifest) && $manifest !== '') {
            load_instrumentation($manifest);
        }
    }

    __chronos_bootstrap_instrumentation();
}
