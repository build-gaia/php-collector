<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use JsonSerializable;
use Throwable;

/**
 * Shared cache-read stamping for Laravel events, Symfony cache pools, and Predis.
 * Hit/miss is a boolean attribute; the hit value is the application's own value
 * (unserialized), bounded, fail-open. Misses carry no value — showing an empty
 * payload as if it were the cached document is worse than showing nothing.
 */
final class CacheCapture
{
    public static function encode(mixed $value): string
    {
        if (is_string($value)) {
            return $value;
        }
        if (is_bool($value)) {
            return $value ? 'true' : 'false';
        }
        if ($value === null) {
            return 'null';
        }
        if (is_int($value) || is_float($value)) {
            return (string) $value;
        }
        try {
            if ($value instanceof JsonSerializable || is_array($value) || is_object($value)) {
                $encoded = json_encode(
                    $value,
                    JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE | JSON_INVALID_UTF8_SUBSTITUTE,
                );
                if (is_string($encoded)) {
                    return $encoded;
                }
            }
        } catch (Throwable) {
        }
        if (is_object($value)) {
            return '(object '.$value::class.')';
        }

        return '';
    }

    public static function stamp(Span $span, bool $hit, mixed $value): void
    {
        if ($span->isVoid()) {
            return;
        }
        $span->add('cache.hit', $hit ? 'true' : 'false');
        if ($hit) {
            $span->add('cache.value', self::encode($value), Span::MAX_TEXT_LENGTH);
        }
    }
}
