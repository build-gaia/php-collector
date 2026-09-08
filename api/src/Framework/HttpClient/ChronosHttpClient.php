<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\HttpClient;

use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Propagation;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Symfony\Contracts\HttpClient\HttpClientInterface;
use Symfony\Contracts\HttpClient\ResponseInterface;
use Symfony\Contracts\HttpClient\ResponseStreamInterface;
use Throwable;

/**
 * Decorates a Symfony HttpClientInterface so every outbound call gets a
 * traceparent header and a client span, the same idea as
 * Framework\Psr18\ChronosClientDecorator and Framework\Guzzle\TraceparentMiddleware
 * for the many applications that hold Symfony's own client instead of a PSR-18
 * one or Guzzle directly.
 *
 * The package has zero runtime dependencies, so symfony/http-client-contracts is
 * never required by composer.json. That is safe exactly the way it is for
 * ChronosClientDecorator: PSR-4 autoloading is lazy, and only a caller who
 * already depends on the contracts (to have an inner HttpClientInterface to
 * wrap) can ever reach this file at all.
 *
 * LAZINESS: Symfony's HttpClientInterface is deliberately non-blocking —
 * request() returns a ResponseInterface immediately, before a single byte has
 * gone over the wire, and the transfer only actually happens when something
 * asks the response to resolve (getStatusCode(), getHeaders(), getContent(),
 * toArray(), or iterating it through stream()). A span must not force that
 * transfer just to record it, or wrapping a client would change its
 * performance characteristics (e.g. turn a concurrent multi-request pattern
 * into an accidentally serial one).
 *
 * v1 answer: request() opens the span and returns a ChronosHttpResponse
 * wrapper around the real response. The wrapper stays lazy — it does not touch
 * the inner response at all — and closes the span (stamping
 * http.response.status_code) the first time the CALLER resolves it, via
 * getStatusCode()/getHeaders()/getContent()/toArray()/cancel(). That covers
 * the overwhelmingly common "await this one response" shape without forcing
 * anything eager.
 *
 * KNOWN GAP: stream() is the multiplexed-concurrency escape hatch — many
 * responses driven together, read as an interleaved sequence of chunks rather
 * than through the methods above. This decorator's stream() unwraps
 * ChronosHttpResponse back to the real response so the inner client still
 * recognises its own objects (Symfony's own transports index responses by
 * identity), but it does not itself watch the chunk stream to know when a
 * given response finished. A request whose response is driven exclusively
 * through stream() and never asked for its status/content directly therefore
 * gets a span that opens but is never closed — SpanManager simply drops an
 * unfinished span, so this shows up as a MISSING span rather than a wrong one.
 * Closing spans from inside stream() would mean wrapping ResponseStreamInterface
 * too and mapping each yielded chunk's response back to its span by identity;
 * left for a follow-up since the common case does not need it.
 */
final class ChronosHttpClient implements HttpClientInterface
{
    public function __construct(
        private readonly HttpClientInterface $client,
    ) {
    }

    public function request(string $method, string $url, array $options = []): ResponseInterface
    {
        $options = $this->withTraceparent($options);
        $span = $this->beginSpan($method, $url, $options);

        try {
            $response = $this->client->request($method, $url, $options);
        } catch (Throwable $error) {
            $this->attachException($span, $error);
            $this->finish($span);
            throw $error;
        }

        return new ChronosHttpResponse($response, $span);
    }

    /**
     * Symfony's stream() accepts either one response or an iterable of them and
     * multiplexes their transfer. It must receive the client's OWN response
     * objects — most transports key their internal handle table by object
     * identity — so any ChronosHttpResponse the caller still holds from this
     * client's request() is unwrapped back to the real response first.
     *
     * @param iterable<ResponseInterface>|ResponseInterface $responses
     */
    public function stream(iterable|ResponseInterface $responses, ?float $timeout = null): ResponseStreamInterface
    {
        return $this->client->stream(self::unwrap($responses), $timeout);
    }

    public function withOptions(array $options): static
    {
        return new self($this->client->withOptions($options));
    }

    /** @param iterable<ResponseInterface>|ResponseInterface $responses */
    private static function unwrap(iterable|ResponseInterface $responses): iterable|ResponseInterface
    {
        if ($responses instanceof ChronosHttpResponse) {
            return $responses->unwrap();
        }
        if ($responses instanceof ResponseInterface) {
            return $responses;
        }
        $unwrapped = [];
        foreach ($responses as $response) {
            $unwrapped[] = $response instanceof ChronosHttpResponse ? $response->unwrap() : $response;
        }

        return $unwrapped;
    }

    /**
     * Inject a child traceparent so the callee joins this request's trace.
     * Skipped when the caller already set one, mirroring
     * TraceparentMiddleware's and ChronosClientDecorator's own rule. Symfony
     * accepts `headers` as either an assoc name => value map or a list of
     * "Name: value" strings, so both shapes are checked and preserved.
     *
     * @param array<string, mixed> $options
     * @return array<string, mixed>
     */
    private function withTraceparent(array $options): array
    {
        try {
            if (!self::hasHeader($options, 'traceparent')) {
                $traceparent = NativeExtension::childTraceparent();
                if ($traceparent !== null) {
                    $options = self::addHeader($options, 'traceparent', $traceparent);
                }
            }
            // W3C Trace Context: forwarding traceparent obliges forwarding
            // tracestate too; baggage follows the same forward-as-is rule.
            // Application-set headers are never overwritten, same as above.
            foreach (Propagation::contextHeaders() as $name => $value) {
                if (!self::hasHeader($options, $name)) {
                    $options = self::addHeader($options, $name, $value);
                }
            }

            return $options;
        } catch (Throwable) {
            // A propagation failure must never block the request from being sent.
            return $options;
        }
    }

    /**
     * Append one header while preserving whichever of Symfony's two accepted
     * `headers` shapes the caller used (assoc name => value map, or a list of
     * "Name: value" strings) — converting between them here would surprise any
     * later option merge the application does by shape.
     *
     * @param array<string, mixed> $options
     * @return array<string, mixed>
     */
    private static function addHeader(array $options, string $name, string $value): array
    {
        $headers = $options['headers'] ?? [];
        if (is_array($headers) && self::isList($headers)) {
            $headers[] = $name.': '.$value;
        } else {
            $headers = is_array($headers) ? $headers : [];
            $headers[$name] = $value;
        }
        $options['headers'] = $headers;

        return $options;
    }

    /** @param array<string, mixed> $options */
    private static function hasHeader(array $options, string $name): bool
    {
        $headers = $options['headers'] ?? null;
        if (!is_array($headers)) {
            return false;
        }
        $needle = strtolower($name);
        foreach ($headers as $key => $value) {
            if (is_string($key) && strtolower($key) === $needle) {
                return true;
            }
            if (is_int($key) && is_string($value) && strtolower(explode(':', $value, 2)[0]) === $needle) {
                return true;
            }
        }

        return false;
    }

    /** @param array<mixed> $array */
    private static function isList(array $array): bool
    {
        return $array === [] || array_is_list($array);
    }

    /** @param array<string, mixed> $options */
    private function beginSpan(string $method, string $url, array $options): ?Span
    {
        try {
            $span = SpanManager::open('HTTP '.$method);
            if ($span->isVoid()) {
                return $span;
            }
            $span->add('span.kind', 'client');
            $span->add('http.request.method', $method);
            if ($url !== '') {
                $span->add('url.full', $url);
            }
            $host = self::hostFor($url, $options);
            if ($host !== '') {
                $span->add('server.address', $host);
            }
            $port = self::portFor($url, $options);
            if ($port !== null) {
                $span->add('server.port', (string) $port);
            }

            return $span;
        } catch (Throwable) {
            return null;
        }
    }

    /**
     * $url is frequently relative to a `base_uri` set once via withOptions()
     * (Symfony's own scoped-client pattern), in which case parse_url() alone
     * finds no host on the per-call $url. The base_uri's host is used as a
     * best-effort fallback so a scoped client's calls still carry
     * server.address; url.full still records the literal (possibly relative)
     * $url exactly as the application passed it.
     *
     * @param array<string, mixed> $options
     */
    private static function hostFor(string $url, array $options): string
    {
        $host = (string) (parse_url($url, PHP_URL_HOST) ?? '');
        if ($host !== '') {
            return $host;
        }
        $base = $options['base_uri'] ?? null;

        return is_string($base) ? (string) (parse_url($base, PHP_URL_HOST) ?? '') : '';
    }

    /** @param array<string, mixed> $options */
    private static function portFor(string $url, array $options): ?int
    {
        $port = parse_url($url, PHP_URL_PORT);
        $scheme = (string) (parse_url($url, PHP_URL_SCHEME) ?? '');
        if ($port === null && $scheme === '') {
            $base = $options['base_uri'] ?? null;
            if (is_string($base)) {
                $port = parse_url($base, PHP_URL_PORT);
                $scheme = (string) (parse_url($base, PHP_URL_SCHEME) ?? '');
            }
        }
        if (is_int($port)) {
            return $port;
        }

        return match (strtolower($scheme)) {
            'http' => 80,
            'https' => 443,
            default => null,
        };
    }

    private function attachException(?Span $span, Throwable $exception): void
    {
        if (!$span instanceof Span) {
            return;
        }
        try {
            $span->add('error.type', get_class($exception));
            $span->recordException($exception, false);
        } catch (Throwable) {
        }
    }

    /**
     * Only reached when $this->client->request() itself throws synchronously
     * (a malformed URL/options, no transport available) — before there is any
     * response for a ChronosHttpResponse to later close the span from.
     */
    private function finish(?Span $span): void
    {
        if (!$span instanceof Span) {
            return;
        }
        try {
            $span->finish();
        } catch (Throwable) {
        }
    }
}
