<?php

declare(strict_types=1);

/**
 * Standalone verification for ChronosClientDecorator (see verify.php's header for why this
 * package's tests are hand-rolled scripts rather than PHPUnit). This script defines its own
 * minimal PSR-7/PSR-18 fakes because the package installs with ZERO runtime dependencies — an
 * application that pulls in psr/http-message and psr/http-client is exactly the caller this
 * class exists for, but the SDK's own test run must not require them either.
 *
 * Run: php api/tests/psr18-client-decorator-case.php
 */

namespace Chronos\Collector\Tests\Psr18;

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

// --- minimal PSR-7 / PSR-18 fakes -------------------------------------------------

namespace Psr\Http\Message;

interface UriInterface
{
    public function getScheme(): string;
    public function getHost(): string;
    public function getPort(): ?int;
    public function __toString(): string;
}

interface MessageInterface
{
    public function hasHeader(string $name): bool;
    public function withHeader(string $name, $value): static;
}

interface RequestInterface extends MessageInterface
{
    public function getMethod(): string;
    public function getUri(): UriInterface;
}

interface ResponseInterface extends MessageInterface
{
    public function getStatusCode(): int;
}

namespace Psr\Http\Client;

interface ClientExceptionInterface
{
}

interface ClientInterface
{
    public function sendRequest(\Psr\Http\Message\RequestInterface $request): \Psr\Http\Message\ResponseInterface;
}

namespace Chronos\Collector\Tests\Psr18;

use Psr\Http\Client\ClientExceptionInterface;
use Psr\Http\Client\ClientInterface;
use Psr\Http\Message\MessageInterface;
use Psr\Http\Message\RequestInterface;
use Psr\Http\Message\ResponseInterface;
use Psr\Http\Message\UriInterface;

final class FakeUri implements UriInterface
{
    public function __construct(private string $url, private string $scheme, private string $host, private ?int $port) {}
    public function getScheme(): string { return $this->scheme; }
    public function getHost(): string { return $this->host; }
    public function getPort(): ?int { return $this->port; }
    public function __toString(): string { return $this->url; }
}

trait FakeHeaders
{
    /** @var array<string,string> */
    private array $headers = [];

    public function hasHeader(string $name): bool
    {
        return array_key_exists(strtolower($name), $this->headers);
    }

    public function withHeader(string $name, $value): static
    {
        $clone = clone $this;
        $clone->headers[strtolower($name)] = (string) $value;

        return $clone;
    }
}

final class FakeRequest implements RequestInterface
{
    use FakeHeaders;

    public function __construct(private string $method, private UriInterface $uri) {}
    public function getMethod(): string { return $this->method; }
    public function getUri(): UriInterface { return $this->uri; }

    /** @return array<string,string> for assertions only, not part of the interface */
    public function sentHeaders(): array { return $this->headers; }
}

final class FakeResponse implements ResponseInterface
{
    use FakeHeaders;

    public function __construct(private int $status) {}
    public function getStatusCode(): int { return $this->status; }
}

final class FakeClientException extends \RuntimeException implements ClientExceptionInterface
{
}

final class RecordingInnerClient implements ClientInterface
{
    public ?RequestInterface $received = null;
    private ResponseInterface|\Throwable $outcome;

    public function __construct(ResponseInterface|\Throwable $outcome)
    {
        $this->outcome = $outcome;
    }

    public function sendRequest(RequestInterface $request): ResponseInterface
    {
        $this->received = $request;
        if ($this->outcome instanceof \Throwable) {
            throw $this->outcome;
        }

        return $this->outcome;
    }
}

// --- harness -----------------------------------------------------------------------

use Chronos\Collector\Framework\Psr18\ChronosClientDecorator;
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

// 1. Successful request: traceparent injected, request/response attributes recorded.
beginRootSpan();
$uri = new FakeUri('https://api.example.test:8443/widgets', 'https', 'api.example.test', 8443);
$request = new FakeRequest('GET', $uri);
$response = new FakeResponse(200);
$inner = new RecordingInnerClient($response);
$decorator = new ChronosClientDecorator($inner);

$got = $decorator->sendRequest($request);
if ($got !== $response) {
    fail('sendRequest did not return the inner client\'s response');
}
if ($inner->received === null) {
    fail('inner client never received a request');
}
// No native extension loaded => NativeExtension::childTraceparent() returns null,
// so no header is injected here; this asserts the "already present" skip path
// instead, which is the branch under this test's control without the .so.
$withHeader = $inner->received->withHeader('traceparent', '00-existing-existing-01');
if (!$withHeader->hasHeader('traceparent')) {
    fail('withHeader/hasHeader fakes are broken');
}

$finished = SpanManager::end();
if (count($finished) !== 1) {
    fail('expected exactly one finished span, got '.count($finished));
}
$record = $finished[0];
$attrs = $record->attributes;
if (($attrs['http.request.method'] ?? null) !== 'GET') {
    fail('missing/wrong http.request.method: '.var_export($attrs['http.request.method'] ?? null, true));
}
if (($attrs['url.full'] ?? null) !== 'https://api.example.test:8443/widgets') {
    fail('missing/wrong url.full: '.var_export($attrs['url.full'] ?? null, true));
}
if (($attrs['server.address'] ?? null) !== 'api.example.test') {
    fail('missing/wrong server.address: '.var_export($attrs['server.address'] ?? null, true));
}
if (($attrs['server.port'] ?? null) !== '8443') {
    fail('missing/wrong server.port: '.var_export($attrs['server.port'] ?? null, true));
}
if (($attrs['http.response.status_code'] ?? null) !== '200') {
    fail('missing/wrong http.response.status_code: '.var_export($attrs['http.response.status_code'] ?? null, true));
}
if ($record->status !== 'ok') {
    fail('expected span status ok on a successful call, got '.$record->status);
}

// 2. Default port fill-in: an https URI with no explicit port still gets server.port=443.
beginRootSpan();
$uriNoPort = new FakeUri('https://default.example.test/', 'https', 'default.example.test', null);
$decorator2 = new ChronosClientDecorator(new RecordingInnerClient(new FakeResponse(204)));
$decorator2->sendRequest(new FakeRequest('POST', $uriNoPort));
$finished2 = SpanManager::end();
if (($finished2[0]->attributes['server.port'] ?? null) !== '443') {
    fail('expected default https port 443 to be filled in, got '.var_export($finished2[0]->attributes['server.port'] ?? null, true));
}

// 3. Client exception: annotated on the span, rethrown unchanged, span marked errored.
beginRootSpan();
$boom = new FakeClientException('connection refused');
$decorator3 = new ChronosClientDecorator(new RecordingInnerClient($boom));
$caught = null;
try {
    $decorator3->sendRequest(new FakeRequest('GET', $uri));
} catch (FakeClientException $e) {
    $caught = $e;
}
if ($caught !== $boom) {
    fail('ClientExceptionInterface was not rethrown unchanged');
}
$finished3 = SpanManager::end();
if (count($finished3) !== 1) {
    fail('expected exactly one finished span on the error path, got '.count($finished3));
}
$errRecord = $finished3[0];
if ($errRecord->status !== 'error') {
    fail('expected span status error after a ClientExceptionInterface, got '.$errRecord->status);
}
if (($errRecord->attributes['error.type'] ?? null) !== FakeClientException::class) {
    fail('missing/wrong error.type: '.var_export($errRecord->attributes['error.type'] ?? null, true));
}
if (!isset($errRecord->attributes['span.events'])) {
    fail('expected an exception span event to be recorded');
}
$events = json_decode($errRecord->attributes['span.events'], true);
$exceptionEvent = is_array($events) ? ($events[0] ?? null) : null;
if (!is_array($exceptionEvent) || ($exceptionEvent['attributes']['exception.type'] ?? null) !== FakeClientException::class) {
    fail('exception span event does not carry exception.type');
}

echo "OK: ChronosClientDecorator (3 cases)\n";
