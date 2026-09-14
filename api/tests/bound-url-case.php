<?php

declare(strict_types=1);

/**
 * Bound inbound URL attributes the framework bridges stamp onto the request
 * root. `http.route` is the template; `url.path` / `url.full` are the instance.
 *
 * No PHPUnit, no vendor/: run with `php api/tests/bound-url-case.php`.
 */

namespace Chronos\Collector\Tests;

use Chronos\Collector\Service\BoundUrl;

$root = __DIR__.'/..';
spl_autoload_register(static function (string $class) use ($root): void {
    $prefix = 'Chronos\\Collector\\';
    if (!str_starts_with($class, $prefix)) {
        return;
    }
    $path = $root.'/src/'.str_replace('\\', '/', substr($class, strlen($prefix))).'.php';
    if (is_file($path)) {
        require $path;
    }
});

$failures = 0;

function check(bool $condition, string $message): void
{
    global $failures;
    if (!$condition) {
        ++$failures;
        fwrite(STDERR, "FAIL: {$message}\n");
    } else {
        fwrite(STDOUT, "PASS: {$message}\n");
    }
}

check(
    BoundUrl::attributes('organizations/99/orders/70', 'https://oms.qls.local/organizations/99/orders/70')
        === [
            'url.path' => '/organizations/99/orders/70',
            'url.full' => 'https://oms.qls.local/organizations/99/orders/70',
        ],
    'a bound path gets a leading slash and is paired with url.full',
);

check(
    BoundUrl::attributes('/organizations/{organization}/orders/{order}', '') === [],
    'a route template is not stamped as the bound URL',
);

check(
    BoundUrl::attributes('/organizations/99/../admin', 'https://oms.qls.local/organizations/99/../admin') === [],
    'a parent-segment path is dropped rather than recorded',
);

check(
    BoundUrl::attributes('', '') === [],
    'empty values are omitted',
);

fwrite(STDOUT, sprintf("\n%d failure(s)\n", $failures));
exit($failures === 0 ? 0 : 1);
