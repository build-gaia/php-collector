<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\HttpClient;

use Symfony\Contracts\HttpClient\ResponseInterface;

/**
 * A thin ResponseInterface pass-through that stays exactly as lazy as the
 * response it wraps (see ChronosHttpClient's class doc for why laziness
 * matters). Every method delegates to the inner response unchanged; the only
 * addition is closing the client span the first time a caller actually
 * resolves the response — on success AND on the exception paths, see the
 * ChronosHttpResponseMethods trait, which holds the entire implementation.
 *
 * TWO declarations of the same class, chosen by `interface_exists()`, because
 * the interface this wrapper must not silently drop lives in a different
 * package than the one it must not depend on being present:
 *
 * Symfony's concrete transport responses all implement StreamableInterface
 * (from symfony/http-client, NOT the contracts package), and application code
 * legitimately calls `$response->toStream()` on whatever `http_client` hands
 * back — a documented feature. ChronosIntegrationsPass decorates `http_client`
 * unconditionally, so a wrapper that did not declare the interface would turn
 * that working call into an instanceof-check miss (StreamWrapper::createResource
 * without the $client argument, typed consumers) with zero application change.
 * But this file must also load for a caller who holds only the CONTRACTS
 * package (any custom HttpClientInterface, no symfony/http-client installed),
 * where the interface does not exist and implementing it would be a fatal.
 * Hence the branch — same conditional-declaration technique as
 * Framework/Messenger/ChronosMiddleware. Both branches carry toStream() via
 * the trait, so duck-typed callers work either way; the interface_exists()
 * branch additionally satisfies instanceof checks.
 */
if (interface_exists(\Symfony\Component\HttpClient\Response\StreamableInterface::class)) {
    final class ChronosHttpResponse implements ResponseInterface, \Symfony\Component\HttpClient\Response\StreamableInterface
    {
        use ChronosHttpResponseMethods;
    }
} else {
    final class ChronosHttpResponse implements ResponseInterface
    {
        use ChronosHttpResponseMethods;
    }
}
