<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * Process-wide registry of user-declared method decorations, keyed "Class::method". This mirrors the
 * ddtrace pattern where an application ships a separate instrumentation manifest that calls
 * \DDTrace\trace_method(...) to attach a span-shaping closure to a method it does not own. Chronos\
 * trace_method() (see src/functions.php) writes here; SpanManager::callThrough() reads here to wrap a
 * userland call in a child span.
 *
 * There is deliberately no invocation logic in this class: it is pure storage so the future native
 * extension (which will intercept calls transparently via zend_observer_fcall_register) and today's
 * explicit userland call-through share exactly one source of truth for what is decorated.
 */
final class SpanDecorations
{
    /** @var array<string, callable> */
    private static array $decorators = [];

    public static function register(string $class, string $method, callable $decorator): void
    {
        $key = self::key($class, $method);
        if ($key === null) {
            return;
        }
        self::$decorators[$key] = $decorator;
    }

    public static function lookup(string $class, string $method): ?callable
    {
        $key = self::key($class, $method);
        if ($key === null) {
            return null;
        }

        return self::$decorators[$key] ?? null;
    }

    /** @return array<string, callable> */
    public static function all(): array
    {
        return self::$decorators;
    }

    public static function clear(): void
    {
        self::$decorators = [];
    }

    private static function key(string $class, string $method): ?string
    {
        $class = ltrim(trim($class), '\\');
        $method = trim($method);
        if ($class === '' || $method === '') {
            return null;
        }

        return $class.'::'.$method;
    }
}
