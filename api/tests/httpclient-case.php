<?php

declare(strict_types=1);

/**
 * Standalone verification for ChronosHttpClient / ChronosHttpResponse (see verify.php's header
 * for why this package's tests are hand-rolled scripts rather than PHPUnit). This script defines
 * its own minimal symfony/http-client-contracts fakes because the package installs with ZERO
 * runtime dependencies — an application that pulls in symfony/http-client is exactly the caller
 * this decorator exists for, but the SDK's own test run must not require it either.
 *
 * Run: php api/tests/httpclient-case.php
 */

namespace Chronos\Collector\Tests\HttpClient;

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

// --- minimal symfony/http-client-contracts fakes ----------------------------------

namespace Symfony\Contracts\HttpClient;

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

namespace Chronos\Collector\Tests\HttpClient;

use Symfony\Contracts\HttpClient\HttpClientInterface;
use Symfony\Contracts\HttpClient\ResponseInterface;
use Symfony\Contracts\HttpClient\ResponseStreamInterface;

final class FakeResponse implements ResponseInterface
{
    public int $statusCodeCalls = 0;

    public function __construct(
        private readonly int $status,
        private readonly string $content = '',
        private readonly ?\Throwable $throwsOnStatus = null,
    ) {
    }

    public function getStatusCode(): int
    {
        ++$this->statusCodeCalls;
        if ($this->throwsOnStatus !== null) {
            throw $this->throwsOnStatus;
        }

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
        return null;
    }
}

final class FakeResponseStream implements ResponseStreamInterface
{
}

final class RecordingInnerClient implements HttpClientInterface
{
    /** @var array{0: string, 1: string, 2: array<string, mixed>}|null */
    public ?array $received = null;

    /** @var array<int, ResponseInterface> */
    public array $streamedWith = [];

    private ResponseInterface|\Throwable $outcome;

    public function __construct(ResponseInterface|\Throwable $outcome)
    {
        $this->outcome = $outcome;
    }

    public function request(string $method, string $url, array $options = []): ResponseInterface
    {
        $this->received = [$method, $url, $options];
        if ($this->outcome instanceof \Throwable) {
            throw $this->outcome;
        }

        return $this->outcome;
    }

    public function stream(iterable|ResponseInterface $responses, ?float $timeout = null): ResponseStreamInterface
    {
        $this->streamedWith = $responses instanceof ResponseInterface ? [$responses] : [...$responses];

        return new FakeResponseStream();
    }

    public function withOptions(array $options): static
    {
        return $this;
    }
}

// --- harness -----------------------------------------------------------------------

use Chronos\Collector\Framework\HttpClient\ChronosHttpClient;
use Chronos\Collector\Framework\HttpClient\ChronosHttpResponse;
use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Chronos\Collector\Service\TraceContext;

function beginRootSpan(): void
{
    // NativeExtension::loaded() is false in this bare test process (no .so), so
    // SpanManager::complete() falls back to its own static buffer instead of the
    // FFI bridge — exactly the pure-PHP path this test exercises.
    NativeExtension::reset();
    $root = Span::open(bin2hex(random_bytes(16)), TraceContext::newSpanId(), '', 'root');
    SpanManager::begin($root);
}

function fail(string $message): void
{
    fwrite(STDERR, "FAIL: {$message}\n");
    exit(1);
}

// 1. Laziness: request() must not resolve the span before the caller asks for
// a status code — no finished span exists until getStatusCode() is called.
beginRootSpan();
$inner1 = new RecordingInnerClient(new FakeResponse(200));
$client1 = new ChronosHttpClient($inner1);
$wrapped1 = $client1->request('GET', 'https://api.example.test:8443/widgets');
if (!$wrapped1 instanceof ChronosHttpResponse) {
    fail('request() did not return a ChronosHttpResponse');
}
$midFlight = SpanManager::end();
if ($midFlight !== []) {
    fail('span closed before the caller resolved the response');
}

// Redo with the span left open across end(), by re-beginning and reading the status.
beginRootSpan();
$inner1b = new RecordingInnerClient(new FakeResponse(200));
$client1b = new ChronosHttpClient($inner1b);
$wrapped1b = $client1b->request('GET', 'https://api.example.test:8443/widgets');
$code = $wrapped1b->getStatusCode();
if ($code !== 200) {
    fail('getStatusCode() did not delegate to the inner response');
}
$finished1 = SpanManager::end();
if (count($finished1) !== 1) {
    fail('expected exactly one finished span once the response resolved, got '.count($finished1));
}
$attrs1 = $finished1[0]->attributes;
if (($attrs1['http.request.method'] ?? null) !== 'GET') {
    fail('missing/wrong http.request.method: '.var_export($attrs1['http.request.method'] ?? null, true));
}
if (($attrs1['url.full'] ?? null) !== 'https://api.example.test:8443/widgets') {
    fail('missing/wrong url.full: '.var_export($attrs1['url.full'] ?? null, true));
}
if (($attrs1['server.address'] ?? null) !== 'api.example.test') {
    fail('missing/wrong server.address: '.var_export($attrs1['server.address'] ?? null, true));
}
if (($attrs1['server.port'] ?? null) !== '8443') {
    fail('missing/wrong server.port: '.var_export($attrs1['server.port'] ?? null, true));
}
if (($attrs1['http.response.status_code'] ?? null) !== '200') {
    fail('missing/wrong http.response.status_code: '.var_export($attrs1['http.response.status_code'] ?? null, true));
}
if ($finished1[0]->status !== 'ok') {
    fail('expected span status ok on a 200, got '.$finished1[0]->status);
}
if ($inner1b->received === null || $inner1b->received[0] !== 'GET') {
    fail('inner client never received the request');
}
// Calling getStatusCode() a second time must not double-count the close.
$wrapped1b->getStatusCode();
if ($inner1b->received === null) {
    fail('sanity: request should have been recorded');
}

// 2. Default port fill-in: an https URL with no explicit port still gets server.port=443.
beginRootSpan();
$inner2 = new RecordingInnerClient(new FakeResponse(204));
$client2 = new ChronosHttpClient($inner2);
$client2->request('POST', 'https://default.example.test/')->getStatusCode();
$finished2 = SpanManager::end();
if (($finished2[0]->attributes['server.port'] ?? null) !== '443') {
    fail('expected default https port 443, got '.var_export($finished2[0]->attributes['server.port'] ?? null, true));
}

// 3. A synchronous exception from request() itself is rethrown unchanged and
// still closes the span with an error status (no ChronosHttpResponse exists
// yet to close it, since there is no response).
beginRootSpan();
$boom = new \RuntimeException('connect() timed out');
$inner3 = new RecordingInnerClient($boom);
$client3 = new ChronosHttpClient($inner3);
$caught = null;
try {
    $client3->request('GET', 'https://api.example.test/widgets');
} catch (\RuntimeException $e) {
    $caught = $e;
}
if ($caught !== $boom) {
    fail('the inner exception was not rethrown unchanged');
}
$finished3 = SpanManager::end();
if (count($finished3) !== 1) {
    fail('expected exactly one finished span on the error path, got '.count($finished3));
}
if ($finished3[0]->status !== 'error') {
    fail('expected span status error after a request()-time exception, got '.$finished3[0]->status);
}
if (($finished3[0]->attributes['error.type'] ?? null) !== \RuntimeException::class) {
    fail('missing/wrong error.type: '.var_export($finished3[0]->attributes['error.type'] ?? null, true));
}

// 4. A 500 response marks the span errored even though nothing threw.
beginRootSpan();
$inner4 = new RecordingInnerClient(new FakeResponse(503));
$client4 = new ChronosHttpClient($inner4);
$client4->request('GET', 'https://api.example.test/widgets')->getStatusCode();
$finished4 = SpanManager::end();
if ($finished4[0]->status !== 'error') {
    fail('expected a 503 to mark the span errored, got '.$finished4[0]->status);
}

// 5. An assoc `headers` option already carrying traceparent is left untouched
// (no native extension loaded here, so this exercises the "already present"
// skip branch the same way psr18-client-decorator-case.php does).
beginRootSpan();
$inner5 = new RecordingInnerClient(new FakeResponse(200));
$client5 = new ChronosHttpClient($inner5);
$client5->request('GET', 'https://api.example.test/x', [
    'headers' => ['Traceparent' => '00-existing-existing-01'],
])->getStatusCode();
$sentHeaders = $inner5->received[2]['headers'] ?? null;
if (($sentHeaders['Traceparent'] ?? null) !== '00-existing-existing-01') {
    fail('an existing assoc traceparent header was altered');
}
SpanManager::end();

// 6. A list-style ("Name: value") `headers` option carrying traceparent is
// also recognised and left untouched.
beginRootSpan();
$inner6 = new RecordingInnerClient(new FakeResponse(200));
$client6 = new ChronosHttpClient($inner6);
$client6->request('GET', 'https://api.example.test/x', [
    'headers' => ['Traceparent: 00-existing-existing-01'],
])->getStatusCode();
$sentHeaders6 = $inner6->received[2]['headers'] ?? null;
if ($sentHeaders6 !== ['Traceparent: 00-existing-existing-01']) {
    fail('an existing list-style traceparent header was altered: '.var_export($sentHeaders6, true));
}
SpanManager::end();

// 7. stream() unwraps a ChronosHttpResponse back to the real response before
// delegating, so the inner client still recognises its own object identity.
beginRootSpan();
$realResponse = new FakeResponse(200);
$inner7 = new RecordingInnerClient($realResponse);
$client7 = new ChronosHttpClient($inner7);
$wrapped7 = $client7->request('GET', 'https://api.example.test/x');
$client7->stream($wrapped7);
if (count($inner7->streamedWith) !== 1 || $inner7->streamedWith[0] !== $realResponse) {
    fail('stream() did not unwrap the ChronosHttpResponse back to the real response');
}
$client7->stream([$wrapped7]);
if (count($inner7->streamedWith) !== 1 || $inner7->streamedWith[0] !== $realResponse) {
    fail('stream() did not unwrap an iterable of ChronosHttpResponse instances');
}
SpanManager::end();

// 8. withOptions() delegates and stays wrapped in a ChronosHttpClient.
beginRootSpan();
$inner8 = new RecordingInnerClient(new FakeResponse(200));
$client8 = new ChronosHttpClient($inner8);
$client8b = $client8->withOptions(['timeout' => 5]);
if (!$client8b instanceof ChronosHttpClient) {
    fail('withOptions() must return a ChronosHttpClient so tracing is not lost');
}
SpanManager::end();

echo "OK: ChronosHttpClient / ChronosHttpResponse (8 cases)\n";
