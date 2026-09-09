<?php

declare(strict_types=1);

/**
 * Standalone verification that wrapping Laravel's view engine stays invisible to
 * the framework's own reflection.
 *
 * Laravel renders its exception page through `BladeMapper::getKnownPaths()`,
 * which reflects on whatever the engine resolver returns for `blade` and reads
 * its private `lastCompiled` list. It tolerates exactly one decorator shape: no
 * `lastCompiled` of its own plus a property named `engine` to unwrap through.
 * A wrapper named anything else makes that walk throw a ReflectionException —
 * and because it happens inside the renderer, the collector's error REPLACES
 * whatever the application actually failed on, which is the worst possible
 * failure mode for instrumentation. This case pins the property name.
 *
 * laravel/framework is not a dependency of this package, so the one contract the
 * engine implements is declared here and the mapper's reflection walk is
 * reproduced verbatim from the framework source.
 *
 * No PHPUnit, no vendor/: run with `php api/tests/laravel-view-engine-case.php`.
 */

namespace Illuminate\Contracts\View {
    interface Engine
    {
        /**
         * @param  string  $path
         * @param  array<mixed>  $data
         */
        public function get($path, array $data = []): string;
    }
}

namespace Chronos\Collector\Tests\Fixtures {
    /** Stands in for Blade's CompilerEngine: the private list the mapper wants. */
    class CompilerEngine implements \Illuminate\Contracts\View\Engine
    {
        /** @var array<int, string> */
        private array $lastCompiled = [];

        public function __construct(private readonly object $compiler)
        {
        }

        /**
         * @param  string  $path
         * @param  array<mixed>  $data
         */
        public function get($path, array $data = []): string
        {
            $this->lastCompiled[] = (string) $path;

            return 'rendered';
        }

        public function getCompiler(): object
        {
            return $this->compiler;
        }
    }

    class Compiler
    {
        public function getCompiledPath(string $path): string
        {
            return $path . '.compiled';
        }
    }
}

namespace Chronos\Collector\Tests {

    use Chronos\Collector\Framework\Laravel\ChronosViewEngine;
    use Chronos\Collector\Tests\Fixtures\Compiler;
    use Chronos\Collector\Tests\Fixtures\CompilerEngine;
    use ReflectionClass;
    use ReflectionProperty;
    use Throwable;

    $root = __DIR__.'/..';

    spl_autoload_register(static function (string $class) use ($root): void {
        $prefix = 'Chronos\\Collector\\';
        if (!str_starts_with($class, $prefix)) {
            return;
        }
        $path = $root.'/src/'.str_replace('\\', '/', substr($class, strlen($prefix))).'.php';
        if (is_file($path)) {
            require $path;
        }
    });

    $failures = 0;
    $check = static function (bool $condition, string $message) use (&$failures): void {
        if ($condition) {
            fwrite(STDOUT, "PASS {$message}\n");

            return;
        }
        $failures++;
        fwrite(STDOUT, "FAIL {$message}\n");
    };

    $inner = new CompilerEngine(new Compiler());
    $wrapped = new ChronosViewEngine($inner);
    $wrapped->get('/app/resources/views/dashboard.blade.php', ['user' => 1]);

    // Reproduced from Illuminate\Foundation\Exceptions\Renderer\Mappers\BladeMapper.
    $lastCompiled = null;
    $thrown = null;
    try {
        $reflection = new ReflectionClass($wrapped);
        if (!$reflection->hasProperty('lastCompiled') && $reflection->hasProperty('engine')) {
            $engine = $reflection->getProperty('engine')->getValue($wrapped);
            $lastCompiled = (new ReflectionProperty($engine, 'lastCompiled'))->getValue($engine);
        } else {
            $lastCompiled = $reflection->getProperty('lastCompiled')->getValue($wrapped);
        }
    } catch (Throwable $exception) {
        $thrown = $exception;
    }

    $check(
        $thrown === null,
        'the exception renderer can reflect through the wrapper: ' . ($thrown?->getMessage() ?? 'no throw'),
    );
    $check(
        $lastCompiled === ['/app/resources/views/dashboard.blade.php'],
        'and reaches the real engine\'s compiled list, not the wrapper\'s',
    );
    $check(
        $wrapped->getCompiler() instanceof Compiler,
        'getCompiler() still forwards, so view caching and Blade components keep working',
    );

    fwrite(STDOUT, sprintf("\n%d failure(s)\n", $failures));
    exit($failures === 0 ? 0 : 1);
}
