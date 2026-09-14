<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * Bound inbound URL attributes, kept apart from `http.route`.
 *
 * Framework bridges stamp the route template onto the request root at
 * request-end (`organizations/{organization}/orders/{order}`). The instance
 * this request actually served (`/organizations/99/orders/70`) belongs on
 * `url.path` / `url.full`. Empty or templated values are omitted so an older
 * native capture is not overwritten with a pattern.
 */
final class BoundUrl
{
    /**
     * @return array<string, string>
     */
    public static function attributes(string $path = '', string $full = ''): array
    {
        $attributes = [];
        $path = self::boundPath($path);
        if ($path !== '') {
            $attributes['url.path'] = $path;
        }
        $full = trim($full);
        if ($full !== '' && !str_contains($full, '{') && !str_contains($full, '..')) {
            $attributes['url.full'] = $full;
        }

        return $attributes;
    }

    private static function boundPath(string $raw): string
    {
        $path = trim($raw);
        if ($path === '' || str_contains($path, '{')) {
            return '';
        }
        if (!str_starts_with($path, '/')) {
            $path = '/'.$path;
        }
        if (str_starts_with($path, '//') || in_array('..', explode('/', $path), true)) {
            return '';
        }

        return $path;
    }
}
