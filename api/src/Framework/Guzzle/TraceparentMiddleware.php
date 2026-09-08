<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Guzzle;

use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Propagation;
use Psr\Http\Message\RequestInterface;

/**
 * Guzzle middleware that injects a W3C traceparent header on every outbound
 * request. Works with any GuzzleHttp\Client, not just the Laravel Http facade.
 *
 * Usage:
 *   $stack = HandlerStack::create();
 *   $stack->push(TraceparentMiddleware::create());
 *   $client = new Client(['handler' => $stack]);
 *
 * The middleware is also auto-registered by ChronosServiceProvider for the
 * Laravel Http facade via Http::globalMiddleware().
 */
final class TraceparentMiddleware
{
    public static function create(): callable
    {
        return static function (callable $handler): callable {
            return static function (RequestInterface $request, array $options) use ($handler) {
                if (!$request->hasHeader('traceparent')) {
                    $traceparent = NativeExtension::childTraceparent();
                    if ($traceparent !== null) {
                        $request = $request->withHeader('traceparent', $traceparent);
                    }
                }
                // W3C Trace Context: a participant that forwards traceparent MUST
                // also forward tracestate it does not understand; baggage follows
                // the same forward-as-is contract. The .so hands both back
                // verbatim as captured on the inbound request. An application-set
                // header is never overwritten, same rule as traceparent above.
                foreach (Propagation::contextHeaders() as $name => $value) {
                    if (!$request->hasHeader($name)) {
                        $request = $request->withHeader($name, $value);
                    }
                }
                return $handler($request, $options);
            };
        };
    }
}
