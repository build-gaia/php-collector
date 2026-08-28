<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\SpanManager;
use Illuminate\Contracts\View\Engine;
use Throwable;

/**
 * A view engine that times the template it renders.
 *
 * Laravel raises `composing:` before a view renders and nothing at all after it,
 * which is why the request root can say WHICH templates rendered but not what
 * any of them cost. The engine is the only place both ends of a render are
 * observable, so the collector wraps the registered engines rather than
 * listening for an event that does not exist.
 *
 * Nesting comes out right for free: `@include` and Blade components render
 * through this same engine, so a partial's span opens while its parent's is
 * still on the stack and the flame chart shows the tree the templates actually
 * form.
 *
 * Everything other than `get()` is forwarded untouched. `CompilerEngine::
 * getCompiler()` is reached for by Blade's own component and cache tooling, and
 * a wrapper that swallowed it would break view caching to buy a span.
 */
final class ChronosViewEngine implements Engine
{
    /** Set once per request by the first render, for the Timeline phase mark. */
    private static bool $renderPhaseMarked = false;

    public function __construct(private readonly Engine $inner)
    {
    }

    public static function resetRequestState(): void
    {
        self::$renderPhaseMarked = false;
    }

    /**
     * @param  string  $path
     * @param  array<mixed>  $data
     */
    public function get($path, array $data = []): string
    {
        // The first template to render is the boundary between the controller's
        // work and the response's: everything after this mark is rendering.
        if (!self::$renderPhaseMarked) {
            self::$renderPhaseMarked = true;
            NativeExtension::markPhase('render');
        }

        $span = SpanManager::open('VIEW '.self::templateName((string) $path));
        if (!$span->isVoid()) {
            $span->add('span.kind', 'internal');
            $span->add('framework.view.template', self::templateName((string) $path));
            $span->add('code.filepath', (string) $path);
            // The COUNT of bound variables, never their values: view data is the
            // application's own domain objects, and the panel that showed them
            // would be a data-exfiltration surface, not an observability one.
            $span->add('framework.view.data.count', (string) count($data));
        }

        try {
            return (string) $this->inner->get($path, $data);
        } catch (Throwable $exception) {
            if (!$span->isVoid()) {
                $span->recordException($exception, false);
            }
            throw $exception;
        } finally {
            $span->finish();
        }
    }

    /**
     * The template's own name, as an author would recognise it: the path relative
     * to the view root with the extension dropped, in dot notation. A compiled
     * Blade path (a cache hash) carries no name at all, so it falls back to the
     * file name rather than presenting the hash as if it meant something.
     *
     * @param  string  $path
     */
    private static function templateName(string $path): string
    {
        if ($path === '') {
            return 'view';
        }
        $name = $path;
        try {
            if (function_exists('config')) {
                foreach ((array) config('view.paths', []) as $root) {
                    if (!is_string($root) || $root === '') {
                        continue;
                    }
                    $root = rtrim($root, '/').'/';
                    if (str_starts_with($name, $root)) {
                        $name = substr($name, strlen($root));
                        break;
                    }
                }
            }
        } catch (Throwable) {
        }
        if ($name === $path) {
            $name = basename($path);
        }
        foreach (['.blade.php', '.php', '.css', '.html'] as $extension) {
            if (str_ends_with($name, $extension)) {
                $name = substr($name, 0, -strlen($extension));
                break;
            }
        }

        return str_replace('/', '.', trim($name, '/'));
    }

    /**
     * Forward anything this wrapper does not implement to the real engine, so
     * wrapping stays invisible to the framework.
     *
     * @param  array<mixed>  $arguments
     */
    public function __call(string $method, array $arguments): mixed
    {
        return $this->inner->{$method}(...$arguments);
    }
}
