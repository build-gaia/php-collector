<?php

declare(strict_types=1);

/**
 * Standalone verification for Framework/Messenger (ChronosTraceparentStamp + ChronosMiddleware).
 *
 * No PHPUnit and no vendor directory (see verify.php's own header for why), and — unlike
 * verify.php — no real symfony/messenger install to run against either: this package has zero
 * runtime dependencies, so the tiniest possible stand-ins for Envelope/StampInterface/
 * MiddlewareInterface/etc. are declared below, just enough of Messenger's real shape (the same
 * method names and return types) for ChronosMiddleware to run against unmodified. They are
 * declared BEFORE the autoloader ever pulls in ChronosMiddleware.php/ChronosTraceparentStamp.php,
 * so the `interface_exists()` guards those files are wrapped in see real classes and the guarded
 * bodies actually declare — proving the "installed" path, not just the "absent" no-op path.
 *
 * The native .so is also not loaded in this environment (`extension_loaded('chronos')` is false
 * here regardless), so NativeExtension is permanently in its fail-open state: requestStart/
 * requestEnd/setRequestAttributes/active() are all no-ops and childTraceparent() always returns
 * null. That is exactly the contract under test for the pass-through paths (a message must reach
 * its handler and a handler's exception must reach the caller whether or not telemetry is doing
 * anything) — but it means the attribute-shaping helpers (destinationName/consumeAttributes/
 * unwrap) can't be observed via a real span, so those are exercised directly through reflection
 * instead, since they are private implementation of otherwise-unobservable behaviour.
 *
 * exit(1) with a message on the first failure, matching this suite's style.
 */

namespace Symfony\Component\Messenger\Stamp {
    interface StampInterface
    {
    }

    final class ReceivedStamp implements StampInterface
    {
        public function __construct(private string $transportName)
        {
        }

        public function getTransportName(): string
        {
            return $this->transportName;
        }
    }

    final class RedeliveryStamp implements StampInterface
    {
        public function __construct(private int $retryCount)
        {
        }

        public function getRetryCount(): int
        {
            return $this->retryCount;
        }
    }

    final class SentStamp implements StampInterface
    {
        public function __construct(private string $senderClass, private ?string $senderAlias = null)
        {
        }

        public function getSenderClass(): string
        {
            return $this->senderClass;
        }

        public function getSenderAlias(): ?string
        {
            return $this->senderAlias;
        }
    }
}

namespace Symfony\Component\Messenger {
    use Symfony\Component\Messenger\Stamp\StampInterface;

    final class Envelope
    {
        /** @var list<StampInterface> */
        private array $stamps;

        /** @param list<StampInterface> $stamps */
        public function __construct(private object $message, array $stamps = [])
        {
            $this->stamps = $stamps;
        }

        public function with(StampInterface ...$stamps): self
        {
            $clone = clone $this;
            foreach ($stamps as $stamp) {
                $clone->stamps[] = $stamp;
            }

            return $clone;
        }

        /** @return list<StampInterface> */
        public function all(?string $stampFqcn = null): array
        {
            if ($stampFqcn === null) {
                return $this->stamps;
            }

            return array_values(array_filter(
                $this->stamps,
                static fn (StampInterface $stamp): bool => $stamp instanceof $stampFqcn,
            ));
        }

        public function last(string $stampFqcn): ?StampInterface
        {
            $matching = $this->all($stampFqcn);

            return $matching === [] ? null : $matching[count($matching) - 1];
        }

        public function getMessage(): object
        {
            return $this->message;
        }
    }
}

namespace Symfony\Component\Messenger\Middleware {
    use Symfony\Component\Messenger\Envelope;

    interface MiddlewareInterface
    {
        public function handle(Envelope $envelope, StackInterface $stack): Envelope;
    }

    interface StackInterface
    {
        public function next(): MiddlewareInterface;
    }
}

namespace Symfony\Component\Messenger\Exception {
    class HandlerFailedException extends \RuntimeException
    {
        /** @param list<\Throwable> $exceptions */
        public function __construct(private object $envelope, private array $exceptions)
        {
            parent::__construct('Handling failed');
        }

        public function getEnvelope(): object
        {
            return $this->envelope;
        }

        /** @return list<\Throwable> */
        public function getExceptions(): array
        {
            return $this->exceptions;
        }
    }
}

namespace Chronos\Collector\Tests {

use Chronos\Collector\Framework\Messenger\ChronosMiddleware;
use Chronos\Collector\Framework\Messenger\ChronosTraceparentStamp;
use Symfony\Component\Messenger\Envelope;
use Symfony\Component\Messenger\Exception\HandlerFailedException;
use Symfony\Component\Messenger\Middleware\MiddlewareInterface;
use Symfony\Component\Messenger\Middleware\StackInterface;
use Symfony\Component\Messenger\Stamp\ReceivedStamp;
use Symfony\Component\Messenger\Stamp\RedeliveryStamp;
use Symfony\Component\Messenger\Stamp\SentStamp;
use Symfony\Component\Messenger\Stamp\StampInterface;

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

/** A fake message class, standing in for an application's own DTO. */
final class FakeMessage
{
}

/** A minimal StackInterface whose next() always returns the same fixed middleware. */
final class FakeStack implements StackInterface
{
    public function __construct(private MiddlewareInterface $inner)
    {
    }

    public function next(): MiddlewareInterface
    {
        return $this->inner;
    }
}

/** Records the envelope it was called with and returns (or throws) a fixed result. */
final class RecordingMiddleware implements MiddlewareInterface
{
    public ?Envelope $seen = null;

    private \Throwable|Envelope $outcome;

    public function __construct(\Throwable|Envelope $outcome)
    {
        $this->outcome = $outcome;
    }

    public function handle(Envelope $envelope, StackInterface $stack): Envelope
    {
        $this->seen = $envelope;
        if ($this->outcome instanceof \Throwable) {
            throw $this->outcome;
        }

        return $this->outcome;
    }
}

final class Runner
{
    private int $failures = 0;

    public function test(string $name, callable $test): void
    {
        try {
            $test();
            fwrite(STDOUT, "PASS {$name}\n");
        } catch (\Throwable $error) {
            ++$this->failures;
            fwrite(STDERR, "FAIL {$name}: {$error->getMessage()}\n");
        }
    }

    public function assertTrue(bool $condition, string $message): void
    {
        if (!$condition) {
            throw new \RuntimeException($message);
        }
    }

    public function assertSame(mixed $expected, mixed $actual, string $context = ''): void
    {
        if ($expected !== $actual) {
            $expectedText = is_scalar($expected) || $expected === null ? var_export($expected, true) : get_debug_type($expected);
            $actualText = is_scalar($actual) || $actual === null ? var_export($actual, true) : get_debug_type($actual);
            throw new \RuntimeException("{$context}expected {$expectedText}, got {$actualText}");
        }
    }

    public function exit(): never
    {
        if ($this->failures > 0) {
            fwrite(STDERR, "{$this->failures} failure(s)\n");
            exit(1);
        }
        fwrite(STDOUT, "All messenger tests passed\n");
        exit(0);
    }
}

/** @return array<string, mixed> attributes read off a private static method via reflection */
function invokePrivateStatic(string $method, mixed ...$args): mixed
{
    $reflection = new \ReflectionMethod(ChronosMiddleware::class, $method);
    $reflection->setAccessible(true);

    return $reflection->invokeArgs(null, $args);
}

$runner = new Runner();

$runner->test('ChronosTraceparentStamp carries the value it was built with and is a real stamp', function () use ($runner): void {
    $stamp = new ChronosTraceparentStamp('00-abc-def-01');
    $runner->assertTrue($stamp instanceof StampInterface, 'stamp must implement StampInterface');
    $runner->assertSame('00-abc-def-01', $stamp->getTraceparent());
});

$runner->test('dispatch with no traceparent available passes the envelope through unchanged', function () use ($runner): void {
    $middleware = new ChronosMiddleware();
    $envelope = new Envelope(new FakeMessage());
    $result = new Envelope(new FakeMessage()); // a distinct object identifies "the handler's own return"
    $inner = new RecordingMiddleware($result);
    $stack = new FakeStack($inner);

    $actual = $middleware->handle($envelope, $stack);

    // No native extension in this process => NativeExtension::childTraceparent() is null =>
    // no stamp is added, so the envelope the inner middleware saw IS the one passed in.
    $runner->assertTrue($inner->seen === $envelope, 'inner middleware must receive the same envelope (no stamp added)');
    $runner->assertTrue($actual === $result, 'dispatch must return exactly what the handler chain returned');
});

$runner->test('dispatch never blocks the send even when span recording finds nothing to record', function () use ($runner): void {
    $middleware = new ChronosMiddleware();
    $envelope = new Envelope(new FakeMessage());
    $inner = new RecordingMiddleware($envelope);
    $stack = new FakeStack($inner);

    // Must not throw: MessagingSpan::published() degrades to a void span when no native
    // extension is loaded, and that must never surface as a dispatch failure.
    $middleware->handle($envelope, $stack);
});

$runner->test('consume passes a ReceivedStamp envelope through and returns the handler result untouched', function () use ($runner): void {
    $middleware = new ChronosMiddleware();
    $envelope = (new Envelope(new FakeMessage()))->with(new ReceivedStamp('async'));
    $result = $envelope->with(new SentStamp(FakeMessage::class));
    $inner = new RecordingMiddleware($result);
    $stack = new FakeStack($inner);

    $actual = $middleware->handle($envelope, $stack);

    $runner->assertTrue($inner->seen === $envelope, 'inner middleware must receive the received envelope unmodified');
    $runner->assertTrue($actual === $result, 'consume must return exactly what the handler chain returned');
});

$runner->test('consume rethrows the handler chain\'s exception unchanged', function () use ($runner): void {
    $middleware = new ChronosMiddleware();
    $envelope = (new Envelope(new FakeMessage()))->with(new ReceivedStamp('async'));
    $boom = new HandlerFailedException($envelope, [new \RuntimeException('boom')]);
    $inner = new RecordingMiddleware($boom);
    $stack = new FakeStack($inner);

    $caught = null;
    try {
        $middleware->handle($envelope, $stack);
    } catch (\Throwable $e) {
        $caught = $e;
    }

    $runner->assertTrue($caught === $boom, 'the exact exception instance must reach the caller, not a copy or a rewrap');
});

$runner->test('destinationName reads the SentStamp alias when the send actually chose a transport', function () use ($runner): void {
    $envelope = (new Envelope(new FakeMessage()))->with(new SentStamp(FakeMessage::class, 'async'));
    $runner->assertSame('async', invokePrivateStatic('destinationName', $envelope));
});

$runner->test('destinationName falls back to the sender class when the SentStamp has no alias', function () use ($runner): void {
    $envelope = (new Envelope(new FakeMessage()))->with(new SentStamp(FakeMessage::class, null));
    $runner->assertSame(FakeMessage::class, invokePrivateStatic('destinationName', $envelope));
});

$runner->test('destinationName falls back to the message class when nothing was ever sent to a transport', function () use ($runner): void {
    $envelope = new Envelope(new FakeMessage());
    $runner->assertSame(FakeMessage::class, invokePrivateStatic('destinationName', $envelope));
});

$runner->test('consumeAttributes carries transport, message name and retry count', function () use ($runner): void {
    $received = new ReceivedStamp('async');
    $envelope = (new Envelope(new FakeMessage()))->with($received, new RedeliveryStamp(3));

    $attributes = invokePrivateStatic('consumeAttributes', $envelope, $received, FakeMessage::class);

    $runner->assertSame('consumer', $attributes['span.kind'] ?? null);
    $runner->assertSame('symfony_messenger', $attributes['messaging.system'] ?? null);
    $runner->assertSame('process', $attributes['messaging.operation'] ?? null);
    $runner->assertSame(FakeMessage::class, $attributes['messaging.message.name'] ?? null);
    $runner->assertSame('async', $attributes['messaging.destination.name'] ?? null);
    $runner->assertSame('3', $attributes['messaging.message.retry_count'] ?? null);
});

$runner->test('consumeAttributes omits retry count on a first attempt (no RedeliveryStamp)', function () use ($runner): void {
    $received = new ReceivedStamp('async');
    $envelope = (new Envelope(new FakeMessage()))->with($received);

    $attributes = invokePrivateStatic('consumeAttributes', $envelope, $received, FakeMessage::class);

    $runner->assertTrue(!array_key_exists('messaging.message.retry_count', $attributes), 'no retry count on a first attempt');
});

$runner->test('unwrap surfaces the real handler exception hiding inside HandlerFailedException', function () use ($runner): void {
    $inner = new \RuntimeException('boom');
    $wrapped = new HandlerFailedException(new Envelope(new FakeMessage()), [$inner]);

    $runner->assertTrue(invokePrivateStatic('unwrap', $wrapped) === $inner, 'unwrap must return the wrapped handler exception, not the wrapper');
});

$runner->test('unwrap leaves a plain exception alone', function () use ($runner): void {
    $plain = new \RuntimeException('plain');

    $runner->assertTrue(invokePrivateStatic('unwrap', $plain) === $plain, 'unwrap must leave a plain exception untouched');
});

$runner->exit();

}
