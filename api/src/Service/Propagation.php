<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * Outbound W3C trace-context forwarding, shared by every HTTP bridge (Guzzle
 * TraceparentMiddleware, the PSR-18 and Symfony HttpClient decorators, the
 * Laravel Http-facade middleware).
 *
 * traceparent itself is NOT handled here: each bridge keeps calling
 * NativeExtension::childTraceparent(), which mints a fresh child span id per
 * outbound call. What this class adds is the rest of the W3C contract — a
 * participant that forwards traceparent MUST also forward tracestate entries it
 * does not understand, and baggage follows the same forward-as-is rule. The .so
 * captured both verbatim on the inbound request and hands them back through
 * chronos_propagation_headers() as {traceparent, tracestate, baggage} with empty
 * strings when absent and UNSPECIFIED key order (it is a Rust HashMap on the
 * other side), so values are always addressed by key, never by position.
 *
 * Guarded on function_exists() alone rather than NativeExtension::loaded():
 * chronos_propagation_headers() only exists when the .so defined it (or a test
 * stubbed the seam deliberately, which is exactly how the standalone tests
 * exercise this class without a build of the extension), so the extra
 * extension_loaded() probe would add nothing but block the test seam.
 */
final class Propagation
{
    /**
     * The headers a bridge forwards verbatim next to its own child traceparent.
     * traceparent is deliberately absent: chronos_propagation_headers() returns
     * the ROOT span's traceparent, meant for consumers with no child-span
     * concept — an HTTP bridge that used it would parent every downstream
     * service onto the root instead of onto its own client span.
     */
    private const FORWARDED = ['tracestate', 'baggage'];

    /**
     * tracestate/baggage to copy onto an outbound request, keyed by header
     * name; entries the .so has no value for are omitted, so a plain
     * `foreach (... as $name => $value)` adds exactly what should be added.
     * Fail-open: any error means "forward nothing", never a broken request.
     *
     * @return array<string, string>
     */
    public static function contextHeaders(): array
    {
        if (!\function_exists('chronos_propagation_headers')) {
            return [];
        }
        try {
            $headers = \chronos_propagation_headers();
        } catch (\Throwable) {
            return [];
        }
        if (!is_array($headers)) {
            return [];
        }
        $forward = [];
        foreach (self::FORWARDED as $name) {
            $value = $headers[$name] ?? '';
            if (is_string($value) && $value !== '') {
                $forward[$name] = $value;
            }
        }

        return $forward;
    }
}
