<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\HttpClient;

use Chronos\Collector\Service\Span;
use Symfony\Contracts\HttpClient\ResponseInterface;
use Throwable;

/**
 * A thin ResponseInterface pass-through that stays exactly as lazy as the
 * response it wraps (see ChronosHttpClient's class doc for why laziness
 * matters). Every method delegates to the inner response unchanged; the only
 * addition is closing the client span the first time a caller actually
 * resolves the response, stamping http.response.status_code at that point.
 *
 * Closing is idempotent and guarded per-instance ($closed) rather than
 * relying on Span's own finished-guard alone, so a caller who calls
 * getStatusCode() and then getContent() only pays the status-code lookup
 * once and the span is stamped from whichever call happened first.
 */
final class ChronosHttpResponse implements ResponseInterface
{
    private bool $closed = false;

    public function __construct(
        private readonly ResponseInterface $response,
        private readonly ?Span $span,
    ) {
    }

    public function getStatusCode(): int
    {
        $code = $this->response->getStatusCode();
        $this->close($code);

        return $code;
    }

    public function getHeaders(bool $throw = true): array
    {
        $headers = $this->response->getHeaders($throw);
        $this->closeFromResponse();

        return $headers;
    }

    public function getContent(bool $throw = true): string
    {
        $content = $this->response->getContent($throw);
        $this->closeFromResponse();

        return $content;
    }

    public function toArray(bool $throw = true): array
    {
        $data = $this->response->toArray($throw);
        $this->closeFromResponse();

        return $data;
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
