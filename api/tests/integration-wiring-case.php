<?php

declare(strict_types=1);

/**
 * Standalone verification for the integrator's wiring layer:
 *
 *   1. Service\Propagation — the shared tracestate/baggage seam over the native
 *      chronos_propagation_headers() function: absent function, well-formed
 *      values, empty strings omitted, key-addressed (order-independent), and
 *      fail-open on a throwing/garbage native answer.
 *   2. The outbound bridges forward tracestate/baggage next to traceparent and
 *      never overwrite an application-set header — exercised through the Guzzle
 *      TraceparentMiddleware (PSR-7 shape) and the Symfony ChronosHttpClient
 *      (both of its accepted `headers` option shapes).
 *   3. Framework\Symfony\ChronosIntegrationsPass — each block registers exactly
 *      when its third-party interface exists and its target service/parameter
 *      is present: Messenger bus-middleware prepend (idempotent), http_client
 *      decoration (skipped when http_client is absent), the doctrine.middleware
 *      tag, and the Monolog pushHandler call.
 *
 * None of symfony/*, doctrine/dbal, monolog or psr/* are dependencies of this
 * package, so — like monolog-psr3-case.php next door — this file declares tiny
 * brace-form namespace fixtures for exactly the types those classes touch, and
 * stubs the native seam with a plain global function (Propagation deliberately
 * guards on function_exists alone so a test can do exactly this).
 *
 * No PHPUnit, no vendor/: run with `php api/tests/integration-wiring-case.php`.
 */

// ---------------------------------------------------------------------------
// Native seam stub: Propagation calls \chronos_propagation_headers() when it
// exists; the test steers it (or makes it blow up) through one global slot.
// ---------------------------------------------------------------------------

namespace {
    $GLOBALS['chronos_propagation_stub'] = [];

    function chronos_propagation_headers(): mixed
    {
        $answer = $GLOBALS['chronos_propagation_stub'];
        if ($answer instanceof \Throwable) {
            throw $answer;
        }

        return $answer;
    }
}

// ---------------------------------------------------------------------------
// PSR-7 fixture (Guzzle middleware speaks RequestInterface).
// ---------------------------------------------------------------------------

namespace Psr\Http\Message {
    interface RequestInterface
    {
        public function hasHeader(string $name): bool;

        public function withHeader(string $name, string $value): static;
    }
}

// ---------------------------------------------------------------------------
// Symfony HttpClient contracts fixture (just enough to declare ChronosHttpClient).
// ---------------------------------------------------------------------------

namespace Symfony\Contracts\HttpClient {
    interface ResponseInterface
    {
    }

    interface ResponseStreamInterface
    {
    }

    interface HttpClientInterface
    {
        public function request(string $method, string $url, array $options = []): ResponseInterface;

        public function stream(iterable|ResponseInterface $responses, ?float $timeout = null): ResponseStreamInterface;

        public function withOptions(array $options): static;
    }
}

// ---------------------------------------------------------------------------
// Symfony DI fixture: the handful of ContainerBuilder/Definition methods the
// compiler pass touches, recording everything for assertion.
// ---------------------------------------------------------------------------

namespace Symfony\Component\DependencyInjection\Compiler {
    interface CompilerPassInterface
    {
        public function process(\Symfony\Component\DependencyInjection\ContainerBuilder $container): void;
    }
}

namespace Symfony\Component\DependencyInjection {
    final class Reference
    {
        public function __construct(public readonly string $id)
        {
        }
    }

    final class Definition
    {
        public ?string $decorates = null;

        public bool $public = false;

        public array $arguments = [];

        public array $tags = [];

        public array $methodCalls = [];

        public function __construct(public readonly string $class)
        {
        }

        public function setDecoratedService(string $id): static
        {
            $this->decorates = $id;

            return $this;
        }

        public function setArguments(array $arguments): static
        {
            $this->arguments = $arguments;

            return $this;
        }

        public function addTag(string $name): static
        {
            $this->tags[] = $name;

            return $this;
        }

        public function setPublic(bool $public): static
        {
            $this->public = $public;

            return $this;
        }

        public function addMethodCall(string $method, array $arguments = []): static
        {
            $this->methodCalls[] = [$method, $arguments];

            return $this;
        }
    }

    final class ContainerBuilder
    {
        /** @var array<string, Definition> */
        public array $definitions = [];

        /** @var array<string, mixed> */
        public array $parameters = [];

        /** @var array<string, array<int, array<string, mixed>>> id => tags */
        public array $taggedServices = [];

        /** @var list<string> */
        public array $aliases = [];

        /** @var list<object> */
        public array $compilerPasses = [];

        public function register(string $id, string $class): Definition
        {
            return $this->definitions[$id] = new Definition($class);
        }

        public function addCompilerPass(object $pass, string $type = '', int $priority = 0): static
        {
            $this->compilerPasses[] = $pass;

            return $this;
        }

        public function hasDefinition(string $id): bool
        {
            return isset($this->definitions[$id]);
        }

        public function getDefinition(string $id): Definition
        {
            return $this->definitions[$id];
        }

        public function hasAlias(string $id): bool
        {
            return in_array($id, $this->aliases, true);
        }

        public function hasParameter(string $name): bool
        {
            return array_key_exists($name, $this->parameters);
        }

        public function getParameter(string $name): mixed
        {
            return $this->parameters[$name];
        }

        public function setParameter(string $name, mixed $value): void
        {
            $this->parameters[$name] = $value;
        }

        /** @return array<string, array<int, array<string, mixed>>> */
        public function findTaggedServiceIds(string $tag): array
        {
            $found = [];
            foreach ($this->taggedServices as $id => $tags) {
                if (in_array($tag, $tags, true)) {
                    $found[$id] = [[]];
                }
            }

            return $found;
        }
    }
}

// ---------------------------------------------------------------------------
// Third-party presence fixtures: each one flips an interface_exists()/
// class_exists() guard in ChronosIntegrationsPass to "installed".
// ---------------------------------------------------------------------------

namespace Symfony\Component\HttpKernel\Bundle {
    abstract class Bundle
    {
        public function build(\Symfony\Component\DependencyInjection\ContainerBuilder $container): void
        {
        }
    }
}

namespace Symfony\Component\DependencyInjection\Compiler {
    final class PassConfig
    {
        public const TYPE_BEFORE_OPTIMIZATION = 'beforeOptimization';
    }
}

namespace Symfony\Component\Messenger\Middleware {
    interface MiddlewareInterface
    {
    }
}

namespace Doctrine\DBAL\Driver {
    interface Middleware
    {
    }
}

namespace Monolog {
    class Logger
    {
    }
}

namespace Monolog\Handler {
    abstract class AbstractProcessingHandler
    {
    }
}

// ---------------------------------------------------------------------------
// Laravel fixtures: just enough of ServiceProvider/Log for
// ChronosServiceProvider::boot() to run to completion (section 6 below).
// ---------------------------------------------------------------------------

namespace Illuminate\Support {
    abstract class ServiceProvider
    {
        public function __construct(protected $app)
        {
        }
    }
}

namespace Illuminate\Support\Facades {
    /**
     * Stands in for Laravel's Log facade. `getLogger()` returns whatever the test
     * parked in the global slot (a RecordingMonologLogger), matching the real
     * facade's own contract: it forwards to the default channel's actual
     * Monolog\Logger.
     */
    final class Log
    {
        public static function listen(callable $callback): void
        {
            $GLOBALS['chronos_log_listen_calls'][] = $callback;
        }

        public static function getLogger(): object
        {
            return $GLOBALS['chronos_fake_monolog_logger'];
        }
    }
}

namespace Chronos\Collector\Tests\Fixtures {
    /** Records every pushHandler() call, standing in for the real Monolog\Logger. */
    final class RecordingMonologLogger
    {
        /** @var list<object> */
        public array $pushed = [];

        public function pushHandler(object $handler): static
        {
            $this->pushed[] = $handler;

            return $this;
        }
    }

    /**
     * Minimal Laravel container: enough for ChronosServiceProvider::boot() to run
     * to completion. Every OTHER optional wiring path in boot() (Kernel
     * middleware, view-engine instrumentation, the exception reportable hook) is
     * already wrapped in its own try/catch in the provider — see that class's own
     * doc blocks — so a make() that always throws exercises exactly those
     * fail-open paths without a real container standing behind them.
     */
    final class FakeLaravelApp
    {
        public function make(string $abstract): mixed
        {
            throw new \RuntimeException("not bound in this fixture: {$abstract}");
        }

        public function booted(callable $callback): void
        {
        }

        public function afterResolving(string $abstract, callable $callback): void
        {
        }

        public function resolved(string $abstract): bool
        {
            return false;
        }
    }
}

// ---------------------------------------------------------------------------
// The test proper.
// ---------------------------------------------------------------------------

namespace Chronos\Collector\Tests\IntegrationWiring {

    use Chronos\Collector\Framework\Guzzle\TraceparentMiddleware;
    use Chronos\Collector\Framework\HttpClient\ChronosHttpClient;
    use Chronos\Collector\Framework\Laravel\ChronosServiceProvider;
    use Chronos\Collector\Framework\Laravel\QueueTelemetry;
    use Chronos\Collector\Framework\Laravel\RichTelemetryHooks;
    use Chronos\Collector\Framework\Monolog\ChronosHandler;
    use Chronos\Collector\Framework\Symfony\ChronosIntegrationsPass;
    use Chronos\Collector\Service\NativeExtension;
    use Chronos\Collector\Service\Propagation;
    use Chronos\Collector\Tests\Fixtures\FakeLaravelApp;
    use Chronos\Collector\Tests\Fixtures\RecordingMonologLogger;
    use Symfony\Component\DependencyInjection\ContainerBuilder;
    use Symfony\Contracts\HttpClient\HttpClientInterface;
    use Symfony\Contracts\HttpClient\ResponseInterface;
    use Symfony\Contracts\HttpClient\ResponseStreamInterface;

    spl_autoload_register(static function (string $class): void {
        $prefix = 'Chronos\\Collector\\';
        if (!str_starts_with($class, $prefix)) {
            return;
        }
        $path = __DIR__.'/../src/'.str_replace('\\', '/', substr($class, strlen($prefix))).'.php';
        if (is_file($path)) {
            require $path;
        }
    });

    $tests = 0;
    $failures = 0;

    function test(string $name, callable $case): void
    {
        global $tests, $failures;
        ++$tests;
        try {
            $case();
            fwrite(STDOUT, "PASS {$name}\n");
        } catch (\Throwable $error) {
            ++$failures;
            fwrite(STDERR, "FAIL {$name}: {$error->getMessage()} ({$error->getFile()}:{$error->getLine()})\n");
        }
    }

    function assertSame(mixed $expected, mixed $actual, string $context): void
    {
        if ($expected !== $actual) {
            throw new \RuntimeException(sprintf(
                '%s: expected %s, got %s',
                $context,
                var_export($expected, true),
                var_export($actual, true),
            ));
        }
    }

    function assertTrue(bool $condition, string $message): void
    {
        if (!$condition) {
            throw new \RuntimeException($message);
        }
    }

    function stubPropagation(mixed $answer): void
    {
        $GLOBALS['chronos_propagation_stub'] = $answer;
    }

    /** Minimal mutable PSR-7 request double for the Guzzle middleware. */
    final class FakeRequest implements \Psr\Http\Message\RequestInterface
    {
        /** @param array<string, string> $headers */
        public function __construct(public array $headers = [])
        {
        }

        public function hasHeader(string $name): bool
        {
            return array_key_exists($name, $this->headers);
        }

        public function withHeader(string $name, string $value): static
        {
            $clone = clone $this;
            $clone->headers[$name] = $value;

            return $clone;
        }
    }

    /** Inner Symfony client double that records the options request() received. */
    final class RecordingClient implements HttpClientInterface
    {
        public array $options = [];

        public function request(string $method, string $url, array $options = []): ResponseInterface
        {
            $this->options = $options;

            return new class implements ResponseInterface {
            };
        }

        public function stream(iterable|ResponseInterface $responses, ?float $timeout = null): ResponseStreamInterface
        {
            throw new \LogicException('not exercised');
        }

        public function withOptions(array $options): static
        {
            return $this;
        }
    }

    // ---- 1. Propagation seam ------------------------------------------------

    test('Propagation forwards tracestate and baggage, addressed by key', function (): void {
        // Deliberately "wrong" order plus the root traceparent the bridges must ignore.
        stubPropagation([
            'baggage' => 'userId=99',
            'traceparent' => '00-aaaa-bbbb-01',
            'tracestate' => 'congo=t61rcWkgMzE',
        ]);
        assertSame(
            ['tracestate' => 'congo=t61rcWkgMzE', 'baggage' => 'userId=99'],
            Propagation::contextHeaders(),
            'forwarded headers',
        );
    });

    test('Propagation omits empty strings (the .so contract for "absent")', function (): void {
        stubPropagation(['traceparent' => '', 'tracestate' => '', 'baggage' => 'a=1']);
        assertSame(['baggage' => 'a=1'], Propagation::contextHeaders(), 'empty tracestate dropped');
        stubPropagation(['traceparent' => '', 'tracestate' => '', 'baggage' => '']);
        assertSame([], Propagation::contextHeaders(), 'all-absent answer');
    });

    test('Propagation is fail-open on garbage and on a throwing native call', function (): void {
        stubPropagation('not-an-array');
        assertSame([], Propagation::contextHeaders(), 'non-array answer');
        stubPropagation(['tracestate' => 42, 'baggage' => ['nested']]);
        assertSame([], Propagation::contextHeaders(), 'non-string values dropped');
        stubPropagation(new \RuntimeException('native blew up'));
        assertSame([], Propagation::contextHeaders(), 'throwing native call');
    });

    // ---- 2. Guzzle middleware forwards, never overwrites ---------------------

    test('Guzzle middleware adds tracestate/baggage and keeps caller-set headers', function (): void {
        stubPropagation(['tracestate' => 'chronos=1', 'baggage' => 'tier=gold']);
        $middleware = TraceparentMiddleware::create();
        $seen = null;
        $handler = $middleware(static function (FakeRequest $request, array $options) use (&$seen) {
            $seen = $request;

            return 'response';
        });

        // Bare request: both context headers appear. No traceparent is expected
        // here — the native extension is not loaded in this process, and the
        // Guzzle middleware (unlike the Laravel hook) mints no PHP fallback.
        $handler(new FakeRequest(), []);
        assertSame('chronos=1', $seen->headers['tracestate'] ?? null, 'tracestate injected');
        assertSame('tier=gold', $seen->headers['baggage'] ?? null, 'baggage injected');

        // Application-set tracestate survives; missing baggage is still filled in.
        $handler(new FakeRequest(['tracestate' => 'mine=1']), []);
        assertSame('mine=1', $seen->headers['tracestate'] ?? null, 'caller tracestate kept');
        assertSame('tier=gold', $seen->headers['baggage'] ?? null, 'baggage still injected');
    });

    // ---- 3. Symfony HttpClient decorator, both header shapes -----------------

    test('ChronosHttpClient forwards context headers in the assoc-map shape', function (): void {
        stubPropagation(['tracestate' => 'chronos=1', 'baggage' => 'tier=gold']);
        $inner = new RecordingClient();
        (new ChronosHttpClient($inner))->request('GET', 'https://api.example.test/x', [
            'headers' => ['Accept' => 'application/json', 'baggage' => 'mine=1'],
        ]);
        $headers = $inner->options['headers'];
        assertSame('chronos=1', $headers['tracestate'] ?? null, 'tracestate added to map');
        assertSame('mine=1', $headers['baggage'] ?? null, 'caller baggage kept');
        assertTrue(!isset($headers[0]), 'map shape preserved');
    });

    test('ChronosHttpClient forwards context headers in the list-of-lines shape', function (): void {
        stubPropagation(['tracestate' => 'chronos=1', 'baggage' => 'tier=gold']);
        $inner = new RecordingClient();
        (new ChronosHttpClient($inner))->request('GET', 'https://api.example.test/x', [
            'headers' => ['Tracestate: mine=1'],
        ]);
        $headers = $inner->options['headers'];
        assertTrue(array_is_list($headers), 'list shape preserved');
        assertTrue(in_array('baggage: tier=gold', $headers, true), 'baggage line appended');
        assertTrue(
            !in_array('tracestate: chronos=1', $headers, true),
            'caller tracestate line (case-insensitive) not duplicated',
        );
    });

    // ---- 4. ChronosIntegrationsPass ------------------------------------------

    function containerWithEverything(): ContainerBuilder
    {
        $container = new ContainerBuilder();
        $container->taggedServices['messenger.bus.default'] = ['messenger.bus'];
        $container->parameters['messenger.bus.default.middleware'] = [['id' => 'validation']];
        $container->register('http_client', 'Symfony\\Component\\HttpClient\\CurlHttpClient');
        $container->register('monolog.logger', 'Monolog\\Logger');

        return $container;
    }

    test('pass wires messenger, http_client, doctrine and monolog when all present', function (): void {
        $container = containerWithEverything();
        (new ChronosIntegrationsPass())->process($container);

        $messenger = 'Chronos\\Collector\\Framework\\Messenger\\ChronosMiddleware';
        $middleware = $container->parameters['messenger.bus.default.middleware'];
        assertSame($messenger, $middleware[0]['id'] ?? null, 'chronos middleware prepended');
        assertSame('validation', $middleware[1]['id'] ?? null, 'existing middleware kept behind it');
        assertTrue($container->hasDefinition($messenger), 'messenger middleware service registered');

        $httpClient = 'Chronos\\Collector\\Framework\\HttpClient\\ChronosHttpClient';
        assertSame('http_client', $container->definitions[$httpClient]->decorates ?? null, 'http_client decorated');

        $doctrine = 'Chronos\\Collector\\Framework\\Doctrine\\ChronosMiddleware';
        assertSame(['doctrine.middleware'], $container->definitions[$doctrine]->tags ?? null, 'doctrine tag added');

        assertTrue($container->hasDefinition('chronos.monolog_handler'), 'monolog handler service registered');
        $calls = $container->getDefinition('monolog.logger')->methodCalls;
        assertSame('pushHandler', $calls[0][0] ?? null, 'handler pushed onto default channel');
        assertSame('chronos.monolog_handler', $calls[0][1][0]->id ?? null, 'push references the handler service');
    });

    test('pass is idempotent: a second run never double-registers', function (): void {
        $container = containerWithEverything();
        $pass = new ChronosIntegrationsPass();
        $pass->process($container);
        $pass->process($container);

        $middleware = $container->parameters['messenger.bus.default.middleware'];
        assertSame(2, count($middleware), 'one chronos entry plus the original');
        assertSame(1, count($container->getDefinition('monolog.logger')->methodCalls), 'one pushHandler call');
    });

    test('pass skips targets that are not configured', function (): void {
        // No http_client, no monolog.logger, a bus with no middleware parameter:
        // an app with none of the optional components must compile untouched
        // except for the tag-only doctrine registration (harmless, tree-shaken).
        $container = new ContainerBuilder();
        $container->taggedServices['messenger.bus.default'] = ['messenger.bus'];
        (new ChronosIntegrationsPass())->process($container);

        assertTrue(
            !$container->hasDefinition('Chronos\\Collector\\Framework\\HttpClient\\ChronosHttpClient'),
            'no http_client decoration without an http_client service',
        );
        assertTrue(!$container->hasDefinition('chronos.monolog_handler'), 'no monolog wiring without monolog.logger');
        assertTrue(
            !$container->hasDefinition('Chronos\\Collector\\Framework\\Messenger\\ChronosMiddleware'),
            'no messenger service without a bus middleware parameter to join',
        );
    });

    // ---- 5. ChronosBundle: the no-extension floor ----------------------------

    test('the Symfony bundle wires nothing at all without the native extension', function (): void {
        // The same floor ChronosServiceProvider::boot() gives Laravel. This suite
        // runs with no .so, which is exactly the case under test; the wired case is
        // covered by section 4, which drives the pass directly.
        assertTrue(!\Chronos\Collector\Service\NativeExtension::loaded(), 'no extension in the test process');

        $container = new ContainerBuilder();
        (new \Chronos\Collector\Framework\Symfony\ChronosBundle())->build($container);

        assertTrue($container->definitions === [], 'no service is registered, http_kernel is left undecorated');
        assertTrue($container->compilerPasses === [], 'no compiler pass is added');
    });

    // ---- 6. Laravel: ChronosServiceProvider wires ChronosHandler onto Monolog ---

    /**
     * Force NativeExtension's memoised process-level answers via reflection — the
     * same technique bunny-case.php's captureBodies() uses, and for the same
     * reason: enabled()/logsEnabled() both gate on loaded() first, and no amount
     * of env makes extension_loaded('chronos') true in this process. Pass null to
     * put a memo back the way it started.
     */
    function forceNativeState(?bool $loadedAndEnabled, ?bool $logsEnabled): void
    {
        (new \ReflectionProperty(NativeExtension::class, 'loaded'))->setValue(null, $loadedAndEnabled);
        (new \ReflectionProperty(NativeExtension::class, 'enabled'))->setValue(null, $loadedAndEnabled);
        (new \ReflectionProperty(NativeExtension::class, 'logsCapture'))->setValue(null, $logsEnabled);
    }

    /**
     * RichTelemetryHooks and QueueTelemetry each install at most once per
     * process; reset the guard so ChronosServiceProvider::boot() can be
     * exercised more than once across the two cases below.
     */
    function resetLaravelInstallGuards(): void
    {
        (new \ReflectionProperty(RichTelemetryHooks::class, 'installed'))->setValue(null, false);
        (new \ReflectionProperty(QueueTelemetry::class, 'installed'))->setValue(null, false);
    }

    function bootChronosServiceProvider(): RecordingMonologLogger
    {
        $GLOBALS['chronos_log_listen_calls'] = [];
        $logger = new RecordingMonologLogger();
        $GLOBALS['chronos_fake_monolog_logger'] = $logger;
        (new ChronosServiceProvider(new FakeLaravelApp()))->boot();

        return $logger;
    }

    test(
        'Laravel: logs enabled pushes ChronosHandler onto Monolog, and RichTelemetryHooks skips its own listener',
        function (): void {
            forceNativeState(true, true);
            resetLaravelInstallGuards();

            $logger = bootChronosServiceProvider();

            assertSame(1, count($logger->pushed), 'exactly one handler pushed onto the Monolog stack');
            assertTrue($logger->pushed[0] instanceof ChronosHandler, 'the pushed handler is ChronosHandler');
            assertSame(
                0,
                count($GLOBALS['chronos_log_listen_calls']),
                'RichTelemetryHooks must not also register Log::listen — every log line would double',
            );

            forceNativeState(null, null);
        },
    );

    test(
        'Laravel: logs disabled (the default) constructs no handler, and RichTelemetryHooks keeps its own listener',
        function (): void {
            forceNativeState(true, false);
            resetLaravelInstallGuards();

            $logger = bootChronosServiceProvider();

            assertSame(0, count($logger->pushed), 'no handler is even constructed when logs are off');
            assertSame(
                1,
                count($GLOBALS['chronos_log_listen_calls']),
                'RichTelemetryHooks still owns log capture when the handler is not wired',
            );

            forceNativeState(null, null);
        },
    );

    if ($failures > 0) {
        fwrite(STDERR, "{$failures} of {$tests} integration-wiring tests failed\n");
        exit(1);
    }
    fwrite(STDOUT, "OK: integration wiring ({$tests} tests)\n");
}
