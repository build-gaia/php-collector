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

use Chronos\Collector\Service\SpanDecorations;

if (!function_exists('Chronos\\trace_method')) {
    /**
     * Register a span decoration for $class::$method. The decorator is invoked with the child span and
     * the call arguments when the method is entered through the call-through path
     * (SpanManager::callThrough), and, once the native extension is installed, transparently on every
     * call. Registration itself never invokes anything and never throws.
     *
     * @param callable $decorator function(\Chronos\Collector\Service\Span $span, array $arguments): void
     */
    function trace_method(string $class, string $method, callable $decorator): void
    {
        SpanDecorations::register($class, $method, $decorator);
    }
}

if (!function_exists('Chronos\\load_instrumentation')) {
    /**
     * Load an application instrumentation manifest — a plain PHP file that calls Chronos\trace_method()
     * to declare its decorations. Fail-open: a missing or unreadable path, or a manifest that throws, is
     * swallowed so instrumentation configuration can never take down the application.
     */
    function load_instrumentation(string $path): void
    {
        try {
            if ($path !== '' && is_file($path) && is_readable($path)) {
                require $path;
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
