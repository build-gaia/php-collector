<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Guzzle;

use Chronos\Collector\Replay\Effect;
use Chronos\Collector\Replay\HttpAnswer;
use Chronos\Collector\Replay\ReplayRuntime;

/**
 * Guzzle middleware that answers outbound HTTP from the active replay recording.
 *
 * When {@see Effect::http()} returns null (not a replay), the request proceeds normally.
 * When it returns a payload, the live handler is never called — mutated fixtures from
 * MutationSweep are therefore consumed without native curl hooks.
 *
 * Usage:
 *   $stack = HandlerStack::create();
 *   $stack->push(ReplayMiddleware::create());
 *   $stack->push(TraceparentMiddleware::create());
 *   $client = new Client(['handler' => $stack]);
 *
 * Requires Guzzle (Promise + Psr7) at runtime. The SDK does not declare that dependency;
 * Laravel / Symfony apps that already use Guzzle already have it.
 */
final class ReplayMiddleware
{
    public static function create(): callable
    {
        return static function (callable $handler): callable {
            return static function ($request, array $options) use ($handler) {
                if (!ReplayRuntime::active()) {
                    return $handler($request, $options);
                }
                $method = method_exists($request, 'getMethod') ? (string) $request->getMethod() : 'GET';
                $uri = method_exists($request, 'getUri') ? (string) $request->getUri() : '';
                $payload = Effect::http($method, $uri);
                if ($payload === null) {
                    return $handler($request, $options);
                }

                return self::fulfilled(self::response($payload));
            };
        };
    }

    /**
     * @param array<string, mixed> $payload
     */
    public static function response(array $payload): object
    {
        $status = HttpAnswer::status($payload);
        $body = HttpAnswer::body($payload);
        $headers = HttpAnswer::headers($payload);
        if (class_exists(\GuzzleHttp\Psr7\Response::class)) {
            return new \GuzzleHttp\Psr7\Response($status, $headers, $body);
        }

        return new SyntheticHttpResponse($status, $body, $headers);
    }

    private static function fulfilled(object $response): mixed
    {
        if (class_exists(\GuzzleHttp\Promise\Create::class)) {
            return \GuzzleHttp\Promise\Create::promiseFor($response);
        }
        if (function_exists('GuzzleHttp\\Promise\\promise_for')) {
            return \GuzzleHttp\Promise\promise_for($response);
        }

        return new ImmediatePromise($response);
    }
}

/**
 * Minimal fulfilled promise when Guzzle Promise is not loaded (unit tests).
 *
 * @internal
 */
final class ImmediatePromise
{
    public function __construct(private mixed $value)
    {
    }

    public function then(?callable $onFulfilled = null, ?callable $onRejected = null): self
    {
        if ($onFulfilled === null) {
            return $this;
        }

        return new self($onFulfilled($this->value));
    }

    public function wait(bool $unwrap = true): mixed
    {
        return $this->value;
    }
}

/**
 * Minimal HTTP response when Guzzle Psr7 is not loaded (unit tests).
 *
 * @internal
 */
final class SyntheticHttpResponse
{
    /**
     * @param array<string, list<string>> $headers
     */
    public function __construct(
        private int $status,
        private string $body,
        private array $headers = [],
    ) {
    }

    public function getStatusCode(): int
    {
        return $this->status;
    }

    public function getBody(): SyntheticHttpBody
    {
        return new SyntheticHttpBody($this->body);
    }

    /** @return array<string, list<string>> */
    public function getHeaders(): array
    {
        return $this->headers;
    }

    /** @return list<string> */
    public function getHeader(string $name): array
    {
        foreach ($this->headers as $header => $values) {
            if (strcasecmp($header, $name) === 0) {
                return $values;
            }
        }

        return [];
    }
}

/**
 * @internal
 */
final class SyntheticHttpBody
{
    public function __construct(private string $contents)
    {
    }

    public function __toString(): string
    {
        return $this->contents;
    }

    public function getContents(): string
    {
        return $this->contents;
    }
}
