<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\HttpClient;

use Symfony\Contracts\HttpClient\ResponseInterface;
use Throwable;

/**
 * The whole body of ChronosHttpResponse, factored into a trait for one reason only:
 * ChronosHttpResponse.php declares the class TWICE behind an `interface_exists()`
 * branch (with and without Symfony's StreamableInterface — see that file's header),
 * and duplicating a hundred lines of delegation per branch would guarantee the two
 * copies drift. The trait is the single implementation; the two declarations are
 * one line each.
 *
 * Two invariants every resolving method here upholds:
 *
 *   1. The span ALWAYS closes when the response resolves — including when the inner
 *      call THROWS. With the default $throw=true, Symfony's lazy response raises
 *      ClientException/ServerException on a 4xx/5xx and TransportException on a
 *      network failure from inside getHeaders()/getContent()/toArray()/toStream(),
 *      which is exactly the moment the transfer's outcome became known. An unfinished
 *      span is silently dropped by SpanManager, so without the catch the FAILING
 *      calls — the ones telemetry exists for — would produce no span and no
 *      error.type at all, while successes recorded fine. Same guarantee the PSR-18
 *      decorator gives with try/finally; here it must live inside each resolving
 *      method because the response, not the send, is where a lazy client fails.
 *
 *   2. Closing is idempotent and guarded per-instance ($closed) rather than relying
 *      on Span's own finished-guard alone, so a caller who calls getStatusCode() and
 *      then getContent() only pays the status-code lookup once and the span is
 *      stamped from whichever call happened first.
 */
trait ChronosHttpResponseMethods
{
    private bool $closed = false;

    public function __construct(
        private readonly ResponseInterface $response,
        private readonly ?\Chronos\Collector\Service\Span $span,
    ) {
    }

    public function getStatusCode(): int
    {
        try {
            $code = $this->response->getStatusCode();
        } catch (Throwable $error) {
            $this->closeFromThrow($error);
            throw $error;
        }
        $this->close($code);

        return $code;
    }

    public function getHeaders(bool $throw = true): array
    {
        try {
            $headers = $this->response->getHeaders($throw);
        } catch (Throwable $error) {
            $this->closeFromThrow($error);
            throw $error;
        }
        $this->closeFromResponse();

        return $headers;
    }

    public function getContent(bool $throw = true): string
    {
        try {
            $content = $this->response->getContent($throw);
        } catch (Throwable $error) {
            $this->closeFromThrow($error);
            throw $error;
        }
        $this->closeFromResponse();

        return $content;
    }

    public function toArray(bool $throw = true): array
    {
        try {
            $data = $this->response->toArray($throw);
        } catch (Throwable $error) {
            $this->closeFromThrow($error);
            throw $error;
        }
        $this->closeFromResponse();

        return $data;
    }

    /**
     * StreamableInterface::toStream(), declared here unconditionally: the method is
     * duck-typed as often as it is interface-checked (`$response->toStream()` straight
     * off the client), so even the branch of ChronosHttpResponse that could not
     * implement the interface still answers the call rather than fataling code that
     * worked before this decorator was auto-wired in.
     *
     * Delegates when the inner response is itself streamable (Symfony's concrete
     * transport responses all are); otherwise falls back to materialising the content
     * into a temp stream — same shape Symfony's own StreamWrapper fallback produces —
     * so the caller still gets a readable resource either way. Resolving the body is
     * what both paths do, so the span closes here like every other resolving method.
     *
     * @return resource
     */
    public function toStream(bool $throw = true)
    {
        if (method_exists($this->response, 'toStream')) {
            try {
                $stream = $this->response->toStream($throw);
            } catch (Throwable $error) {
                $this->closeFromThrow($error);
                throw $error;
            }
            $this->closeFromResponse();

            return $stream;
        }
        // getContent() already carries the close-on-both-outcomes guarantee.
        $content = $this->getContent($throw);
        $stream = fopen('php://temp', 'r+');
        if ($stream === false) {
            throw new \RuntimeException('Unable to open a php://temp stream for the response body.');
        }
        fwrite($stream, $content);
        rewind($stream);

        return $stream;
    }

    public function cancel(): void
    {
        $this->response->cancel();
        try {
            $this->span?->markError();
        } catch (Throwable) {
        }
        $this->close(0);
    }

    public function getInfo(?string $type = null): mixed
    {
        // Deliberately does not close the span: Symfony populates `getInfo()`
        // fields progressively as a transfer proceeds (e.g. on_progress
        // callbacks read it before the body is complete), so treating any
        // getInfo() call as "the response resolved" would close the span far
        // too early and record a status code the transfer had not reached yet.
        return $this->response->getInfo($type);
    }

    /** Unwrap back to the real response, for ChronosHttpClient::stream(). */
    public function unwrap(): ResponseInterface
    {
        return $this->response;
    }

    private function closeFromResponse(): void
    {
        try {
            $this->close($this->response->getStatusCode());
        } catch (Throwable) {
            // A transport error surfaces to the caller from the method they
            // called; the span still deserves to close, just with no code.
            $this->close(0);
        }
    }

    /**
     * The inner response threw while resolving — a 4xx/5xx under $throw=true, or a
     * transport failure. Stamp the error identity, then close with whatever status
     * the transfer DID reach: getInfo('http_code') never forces a resolve (it reads
     * transfer state already known), so a thrown ServerException still records its
     * 500 while a connection that never got a status line closes with none.
     */
    private function closeFromThrow(Throwable $error): void
    {
        if ($this->closed) {
            return;
        }
        try {
            $this->span?->add('error.type', get_class($error));
            $this->span?->markError();
        } catch (Throwable) {
        }
        $statusCode = 0;
        try {
            $known = $this->response->getInfo('http_code');
            $statusCode = is_int($known) ? $known : 0;
        } catch (Throwable) {
        }
        $this->close($statusCode);
    }

    private function close(int $statusCode): void
    {
        if ($this->closed) {
            return;
        }
        $this->closed = true;
        if ($this->span !== null && !$this->span->isVoid() && $statusCode > 0) {
            try {
                $this->span->add('http.response.status_code', (string) $statusCode);
                if ($statusCode >= 500) {
                    $this->span->markError();
                }
            } catch (Throwable) {
            }
        }
        try {
            $this->span?->finish();
        } catch (Throwable) {
        }
    }
}
