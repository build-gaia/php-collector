<?php

declare(strict_types=1);

/**
 * Standalone verification for the 2026-09 review fixes, one behaviour per confirmed finding
 * (see verify.php's header for why this suite is hand-rolled scripts rather than PHPUnit):
 *
 *   1. Messenger dispatch records a producer span ONLY when a SentStamp proves a transport
 *      accepted the message — a sync-handled dispatch is a function call, not a topology edge.
 *   2. Messenger's sync-transport "consume" passes through instead of opening a job-scoped
 *      request inside the live request that dispatched it (the guard's name check is pinned
 *      in messenger-case.php; here the pass-through behaviour is proven end to end).
 *   3. ChronosHttpResponse implements StreamableInterface when it exists and answers
 *      toStream() either way, so `$response->toStream()` callers survive the auto-decoration.
 *   4. A resolving method that THROWS (4xx/5xx with $throw=true, transport error) still
 *      closes the span, stamped with error.type and whatever status the transfer reached.
 *   5. The lazily-closed HTTP client span is opened DETACHED: spans opened between request()
 *      and resolution parent onto the request's own parent, never onto the HTTP span.
 *   6. Severity::fromPsr3/fromSymfony1 give critical/alert/emergency (and notice) their own
 *      OTel severityNumbers (22/23/24, 10) instead of flattening them onto 21 (and 9).
 *
 * The native .so is not loaded here, so SpanManager::complete() falls back to its static
 * buffer — spans are observed by seeding a root with SpanManager::begin() and draining
 * SpanManager::end(), the same technique as httpclient-case.php. Brace-form namespaces
 * because this one file declares fixtures into several vendor namespaces (PHP forbids
 * mixing the two namespace styles in one file). exit(1) on the first failure.
 */

// --- minimal symfony/http-client-contracts fakes, plus the STREAMABLE marker ---------------

namespace Symfony\Contracts\HttpClient {
    interface ResponseInterface
    {
        public function getStatusCode(): int;

        /** @return array<string, string[]> */
        public function getHeaders(bool $throw = true): array;

        public function getContent(bool $throw = true): string;

        /** @return array<mixed> */
        public function toArray(bool $throw = true): array;

        public function cancel(): void;

        public function getInfo(?string $type = null): mixed;
    }

    interface ResponseStreamInterface
    {
    }

    interface HttpClientInterface
    {
        /** @param array<string, mixed> $options */
        public function request(string $method, string $url, array $options = []): ResponseInterface;

        public function stream(iterable|ResponseInterface $responses, ?float $timeout = null): ResponseStreamInterface;

        /** @param array<string, mixed> $options */
        public function withOptions(array $options): static;
    }
}

namespace Symfony\Component\HttpClient\Response {
    // Declared BEFORE ChronosHttpResponse.php autoloads, so its interface_exists()
    // branch takes the streamable declaration — the shape a real symfony/http-client
    // application gets.
    interface StreamableInterface
    {
        /** @return resource */
        public function toStream(bool $throw = true);
    }
}

// --- minimal symfony/messenger fakes (same shapes as messenger-case.php) -------------------

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

        public function last(string $stampFqcn): ?StampInterface
        {
            $matching = array_values(array_filter(
                $this->stamps,
                static fn (StampInterface $stamp): bool => $stamp instanceof $stampFqcn,
            ));

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

// --- fixtures -------------------------------------------------------------------------------

namespace Chronos\Collector\Tests\ReviewFixes {

    use Symfony\Component\Messenger\Envelope;
    use Symfony\Component\Messenger\Middleware\MiddlewareInterface;
    use Symfony\Component\Messenger\Middleware\StackInterface;
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

    /** A response whose resolving methods throw, the way Symfony's do on a 5xx with $throw=true. */
    final class ThrowingResponse implements ResponseInterface
    {
        public function __construct(
            private readonly \Throwable $error,
            private readonly int $reachedStatus,
        ) {
        }

        public function getStatusCode(): int
        {
            // The status line WAS received (it is why getContent() throws) —
            // matching Symfony, where getStatusCode() never throws on a 5xx.
            return $this->reachedStatus;
        }

        public function getHeaders(bool $throw = true): array
        {
            throw $this->error;
        }

        public function getContent(bool $throw = true): string
        {
            throw $this->error;
        }

        public function toArray(bool $throw = true): array
        {
            throw $this->error;
        }

        public function cancel(): void
        {
        }

        public function getInfo(?string $type = null): mixed
        {
            return $type === 'http_code' ? $this->reachedStatus : null;
        }
    }

    /** A well-behaved response; optionally streamable like Symfony's concrete transports. */
    class PlainResponse implements ResponseInterface
    {
        public function __construct(private readonly int $status, private readonly string $content = '')
        {
        }

        public function getStatusCode(): int
        {
            return $this->status;
        }

        public function getHeaders(bool $throw = true): array
        {
            return [];
        }

        public function getContent(bool $throw = true): string
        {
            return $this->content;
        }

        public function toArray(bool $throw = true): array
        {
            return [];
        }

        public function cancel(): void
        {
        }

        public function getInfo(?string $type = null): mixed
        {
            return $type === 'http_code' ? $this->status : null;
        }
    }

    final class StreamableResponse extends PlainResponse
    {
        /** @var resource|null */
        public $handedOut = null;

        /** @return resource */
        public function toStream(bool $throw = true)
        {
            $stream = fopen('php://temp', 'r+');
            $this->handedOut = $stream;

            return $stream;
        }
    }

    final class QueueingInnerClient implements HttpClientInterface
    {
        /** @var list<ResponseInterface> */
        private array $queue;

        public function __construct(ResponseInterface ...$responses)
        {
            $this->queue = array_values($responses);
        }

        public function request(string $method, string $url, array $options = []): ResponseInterface
        {
            $next = array_shift($this->queue);
            if ($next === null) {
                throw new \LogicException('no queued response left');
            }

            return $next;
        }

        public function stream(iterable|ResponseInterface $responses, ?float $timeout = null): ResponseStreamInterface
        {
            return new class implements ResponseStreamInterface {};
        }

        public function withOptions(array $options): static
        {
            return $this;
        }
    }

    final class FakeMessage
    {
    }

    /** A one-middleware stack whose "handler" returns a fixed envelope. */
    final class FixedStack implements StackInterface
    {
        public function __construct(private readonly Envelope $result)
        {
        }

        public function next(): MiddlewareInterface
        {
            return new class($this->result) implements MiddlewareInterface {
                public function __construct(private readonly Envelope $result)
                {
                }

                public function handle(Envelope $envelope, StackInterface $stack): Envelope
                {
                    return $this->result;
                }
            };
        }
    }
}

// --- the cases ------------------------------------------------------------------------------

namespace Chronos\Collector\Tests\ReviewFixes\Run {

    use Chronos\Collector\Framework\HttpClient\ChronosHttpClient;
    use Chronos\Collector\Framework\HttpClient\ChronosHttpResponse;
    use Chronos\Collector\Framework\Messenger\ChronosMiddleware;
    use Chronos\Collector\Service\NativeExtension;
    use Chronos\Collector\Service\Severity;
    use Chronos\Collector\Service\Span;
    use Chronos\Collector\Service\SpanManager;
    use Chronos\Collector\Service\TraceContext;
    use Chronos\Collector\Tests\ReviewFixes\FakeMessage;
    use Chronos\Collector\Tests\ReviewFixes\FixedStack;
    use Chronos\Collector\Tests\ReviewFixes\PlainResponse;
    use Chronos\Collector\Tests\ReviewFixes\QueueingInnerClient;
    use Chronos\Collector\Tests\ReviewFixes\StreamableResponse;
    use Chronos\Collector\Tests\ReviewFixes\ThrowingResponse;
    use Symfony\Component\HttpClient\Response\StreamableInterface;
    use Symfony\Component\Messenger\Envelope;
    use Symfony\Component\Messenger\Stamp\ReceivedStamp;
    use Symfony\Component\Messenger\Stamp\SentStamp;

    function fail(string $message): never
    {
        fwrite(STDERR, "FAIL: {$message}\n");
        exit(1);
    }

    /** @return string the root span id the drained records should be parented on */
    function beginRootSpan(): string
    {
        NativeExtension::reset();
        $root = Span::open(bin2hex(random_bytes(16)), TraceContext::newSpanId(), '', 'root');
        SpanManager::begin($root);

        return $root->id;
    }

    // 1. No SentStamp => no producer span: a sync-handled dispatch is not a topology edge.
    beginRootSpan();
    $middleware = new ChronosMiddleware();
    $envelope = new Envelope(new FakeMessage());
    $middleware->handle($envelope, new FixedStack($envelope)); // handled in-process, never sent
    foreach (SpanManager::end() as $record) {
        if (str_starts_with($record->name, 'PUBLISH')) {
            fail('a dispatch with no SentStamp fabricated a producer span: '.$record->name);
        }
    }

    // 2. SentStamp present => exactly one producer span, named for the TRANSPORT.
    beginRootSpan();
    $sent = $envelope->with(new SentStamp('App\\Transport\\AmqpSender', 'orders_queue'));
    $middleware->handle($envelope, new FixedStack($sent));
    $published = array_values(array_filter(
        SpanManager::end(),
        static fn ($record): bool => str_starts_with($record->name, 'PUBLISH'),
    ));
    if (count($published) !== 1) {
        fail('expected exactly one producer span for a sent message, got '.count($published));
    }
    if (($published[0]->attributes['messaging.destination.name'] ?? null) !== 'orders_queue') {
        fail('producer span must carry the transport alias as messaging.destination.name');
    }
    if (($published[0]->attributes['messaging.operation'] ?? null) !== 'publish') {
        fail('producer span must carry messaging.operation=publish');
    }

    // 3. A sync-transport "consume" is inline handling: pass through, no producer span
    // either (the ReceivedStamp branch), and the handler's own result comes back.
    beginRootSpan();
    $inline = (new Envelope(new FakeMessage()))->with(new ReceivedStamp('sync'));
    $inlineResult = new Envelope(new FakeMessage());
    $actual = $middleware->handle($inline, new FixedStack($inlineResult));
    if ($actual !== $inlineResult) {
        fail('sync-transport consume must return the handler chain\'s own result');
    }
    if (SpanManager::end() !== []) {
        fail('sync-transport consume must record no span of its own');
    }

    // 4. The wrapper is streamable: instanceof for typed/instanceof consumers, and
    // toStream() delegates to the inner response when it can.
    beginRootSpan();
    $streamable = new StreamableResponse(200, 'body');
    $client = new ChronosHttpClient(new QueueingInnerClient($streamable));
    $response = $client->request('GET', 'https://api.example.test/report');
    if (!$response instanceof StreamableInterface) {
        fail('ChronosHttpResponse must implement StreamableInterface when it exists');
    }
    $stream = $response->toStream();
    if (!is_resource($stream) || $stream !== $streamable->handedOut) {
        fail('toStream() must hand back the inner response\'s own stream');
    }
    $streamed = SpanManager::end();
    if (count($streamed) !== 1 || ($streamed[0]->attributes['http.response.status_code'] ?? null) !== '200') {
        fail('toStream() must resolve (and close) the client span like any other resolving method');
    }

    // 5. toStream() still answers for an inner response with no toStream of its own,
    // materialising the content — duck-typed callers survive the decoration either way.
    beginRootSpan();
    $client = new ChronosHttpClient(new QueueingInnerClient(new PlainResponse(200, 'plain body')));
    $stream = $client->request('GET', 'https://api.example.test/x')->toStream();
    if (!is_resource($stream) || stream_get_contents($stream) !== 'plain body') {
        fail('fallback toStream() must yield a readable stream of the body');
    }
    SpanManager::end();

    // 6. A resolving method that THROWS closes the span anyway, stamped with the error
    // and the status the transfer reached — the failing calls are what telemetry is for.
    beginRootSpan();
    $boom = new \RuntimeException('HTTP 503 returned for "https://api.example.test/x".');
    $client = new ChronosHttpClient(new QueueingInnerClient(new ThrowingResponse($boom, 503)));
    $caught = null;
    try {
        $client->request('GET', 'https://api.example.test/x')->getContent();
    } catch (\RuntimeException $e) {
        $caught = $e;
    }
    if ($caught !== $boom) {
        fail('the inner exception must reach the caller unchanged');
    }
    $failed = SpanManager::end();
    if (count($failed) !== 1) {
        fail('expected exactly one finished span for a throwing getContent(), got '.count($failed));
    }
    if ($failed[0]->status !== 'error') {
        fail('a throwing resolve must mark the span errored, got '.$failed[0]->status);
    }
    if (($failed[0]->attributes['error.type'] ?? null) !== \RuntimeException::class) {
        fail('missing/wrong error.type on the throwing resolve');
    }
    if (($failed[0]->attributes['http.response.status_code'] ?? null) !== '503') {
        fail('the status the transfer reached (via getInfo) must still be stamped');
    }

    // 7. Detached parenting: a span opened BETWEEN request() and resolution is a SIBLING
    // of the HTTP span (both children of the root), never its child — and two in-flight
    // requests never nest under each other.
    $rootId = beginRootSpan();
    $client = new ChronosHttpClient(new QueueingInnerClient(new PlainResponse(200), new PlainResponse(201)));
    $first = $client->request('GET', 'https://api.example.test/a');
    $second = $client->request('GET', 'https://api.example.test/b');
    $sql = SpanManager::open('SQL SELECT');
    $sql->finish();
    $first->getStatusCode();
    $second->getStatusCode();
    $records = SpanManager::end();
    if (count($records) !== 3) {
        fail('expected 3 finished spans (SQL + two HTTP), got '.count($records));
    }
    foreach ($records as $record) {
        if ($record->parentSpanId !== $rootId) {
            fail("span '{$record->name}' must parent onto the root, got parent {$record->parentSpanId}");
        }
    }

    // 8. Severity: PSR-3's top three levels each keep their own OTel number, notice too.
    $expectations = [
        'debug' => ['DEBUG', 5],
        'info' => ['INFO', 9],
        'notice' => ['INFO2', 10],
        'warning' => ['WARN', 13],
        'error' => ['ERROR', 17],
        'critical' => ['FATAL2', 22],
        'alert' => ['FATAL3', 23],
        'emergency' => ['FATAL4', 24],
    ];
    foreach ($expectations as $level => [$text, $number]) {
        $pair = Severity::fromPsr3($level);
        if ($pair['text'] !== $text || $pair['number'] !== $number) {
            fail("fromPsr3('{$level}') expected {$text}/{$number}, got {$pair['text']}/{$pair['number']}");
        }
    }
    // The same syslog scale under sfLogger's integer names must agree exactly.
    $symfony1 = [0 => 24, 1 => 23, 2 => 22, 3 => 17, 4 => 13, 5 => 10, 6 => 9, 7 => 5];
    foreach ($symfony1 as $priority => $number) {
        $pair = Severity::fromSymfony1($priority);
        if ($pair['number'] !== $number) {
            fail("fromSymfony1({$priority}) expected {$number}, got {$pair['number']}");
        }
    }
    // An alerting rule on >= 22 now separates emergencies+alerts+criticals from plain FATAL.
    if (!(Severity::fromPsr3('emergency')['number'] >= 22
        && Severity::fromPsr3('critical')['number'] >= 22
        && Severity::fromPsr3('error')['number'] < 22)) {
        fail('the >=22 emergency band must exclude plain errors and include critical+');
    }

    echo "OK: review fixes (8 cases)\n";
}
