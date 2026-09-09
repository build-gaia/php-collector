<?php

declare(strict_types=1);

/**
 * Standalone verification that an authorization check records WHERE the verdict
 * was decided, and that a rendered view records the template file it came from.
 *
 * Both facts are resolved through Laravel types, and laravel/framework is not a
 * dependency of this package, so `verify.php` — which runs with no Illuminate
 * class defined at all — can only prove the absent case. This file defines the
 * one Laravel seam `RequestFacts::policySite()` reads (the `Gate` facade's
 * `getPolicyFor()` / `abilities()`), plus policy fixtures, and asserts the
 * navigable fields land on the catalog record.
 *
 * Brace-form namespaces because the fixtures have to be declared into
 * `Illuminate\Support\Facades` alongside the test code, and PHP forbids mixing
 * the two declaration styles in one file.
 *
 * No PHPUnit, no vendor/: run with `php api/tests/laravel-policy-case.php`.
 */

namespace Illuminate\Support\Facades {
    /**
     * The two reads `policySite()` makes. Neither evaluates an ability: one
     * returns the registered policy instance, the other the ability table.
     */
    class Gate
    {
        /** @var array<string, object> class → policy instance */
        public static array $policies = [];

        /** @var array<string, mixed> ability → closure or "Class@method" */
        public static array $abilities = [];

        public static function getPolicyFor(string $class): ?object
        {
            return self::$policies[$class] ?? null;
        }

        /** @return array<string, mixed> */
        public static function abilities(): array
        {
            return self::$abilities;
        }
    }
}

namespace Chronos\Collector\Tests\Fixtures {
    class MessagePolicy
    {
        public function manageMessages(object $user): bool
        {
            return true;
        }
    }

    class WarehouseGate
    {
        public function manage(object $user): bool
        {
            return false;
        }
    }

    class Template
    {
        public function __construct(private string $path)
        {
        }

        public function name(): string
        {
            return 'errors::403';
        }

        public function getPath(): string
        {
            return $this->path;
        }
    }
}

namespace Chronos\Collector\Tests {
    use Chronos\Collector\Framework\Laravel\RequestFacts;
    use Chronos\Collector\Tests\Fixtures\MessagePolicy;
    use Chronos\Collector\Tests\Fixtures\Template;
    use Chronos\Collector\Tests\Fixtures\WarehouseGate;
    use Illuminate\Support\Facades\Gate;

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

    /**
     * @param array<string, string> $attributes
     * @return array<string, array<string, mixed>> ability → record
     */
    function checksByAbility(array $attributes): array
    {
        $decoded = json_decode($attributes['framework.authorization.checks'] ?? '[]', true);
        $byAbility = [];
        foreach (is_array($decoded) ? $decoded : [] as $record) {
            if (is_array($record)) {
                $byAbility[(string) ($record['name'] ?? '')] = $record;
            }
        }

        return $byAbility;
    }

    // --- A policy class: the method that returned the verdict ------------------

    Gate::$policies = ['App\\Models\\Message' => new MessagePolicy()];
    Gate::$abilities = [];

    RequestFacts::reset();
    RequestFacts::noteGate('manageMessages', true, 'App\\Models\\Message', 'App\\Models\\Message');
    $checks = checksByAbility(RequestFacts::snapshot());

    $policyRecord = $checks['manageMessages'] ?? [];
    check(
        ($policyRecord['policy'] ?? null) === MessagePolicy::class.'::manageMessages',
        'a policy-backed ability names the class and method that decided it',
    );
    check(
        ($policyRecord['code.filepath'] ?? null) === __FILE__,
        'and the file the policy method is declared in',
    );
    check(
        (int) ($policyRecord['code.lineno'] ?? 0) > 0,
        'and the line it starts on',
    );

    // --- A closure ability ----------------------------------------------------

    Gate::$policies = [];
    Gate::$abilities = ['viewDashboard' => static fn (object $user): bool => true];

    RequestFacts::reset();
    RequestFacts::noteGate('viewDashboard', false);
    $closureRecord = checksByAbility(RequestFacts::snapshot())['viewDashboard'] ?? [];

    check(
        ($closureRecord['policy'] ?? null) === 'viewDashboard',
        'a closure ability has no class, so the ability is its own name',
    );
    check(
        ($closureRecord['code.filepath'] ?? null) === __FILE__,
        'but the closure still resolves to a file',
    );

    // --- A "Class@method" ability --------------------------------------------

    Gate::$policies = [];
    Gate::$abilities = ['manageWarehouses' => WarehouseGate::class.'@manage'];

    RequestFacts::reset();
    RequestFacts::noteGate('manageWarehouses', false);
    $stringRecord = checksByAbility(RequestFacts::snapshot())['manageWarehouses'] ?? [];

    check(
        ($stringRecord['policy'] ?? null) === WarehouseGate::class.'::manage',
        'a Class@method ability resolves to the method it names',
    );

    // --- Nothing registered: no guess ----------------------------------------

    Gate::$policies = [];
    Gate::$abilities = [];

    RequestFacts::reset();
    RequestFacts::noteGate('unknownAbility', true, 'App\\Models\\Message');
    $unknownRecord = checksByAbility(RequestFacts::snapshot())['unknownAbility'] ?? [];

    check(
        !isset($unknownRecord['policy']) && !isset($unknownRecord['code.filepath']),
        'an ability that resolves to nothing records no location rather than a guess',
    );
    check(
        ($unknownRecord['result'] ?? null) === 'allow',
        'and the check itself is still recorded in full',
    );

    // --- Views: the template file ---------------------------------------------

    RequestFacts::reset();
    RequestFacts::noteComposedView('composing: errors::403', new Template('/app/resources/views/errors/403.blade.php'));
    RequestFacts::noteComposedView('composing: layouts.nav', null);
    $viewAttributes = RequestFacts::snapshot();

    $templates = json_decode($viewAttributes['framework.views.templates'] ?? '[]', true);
    check(
        is_array($templates) && count($templates) === 1,
        'only a view that reported a path joins the template catalog',
    );
    check(
        ($templates[0]['name'] ?? null) === 'errors::403',
        'the catalog record is keyed by the view name the count map uses',
    );
    check(
        ($templates[0]['code.filepath'] ?? null) === '/app/resources/views/errors/403.blade.php',
        'and carries the compiled template path',
    );
    check(
        ($viewAttributes['framework.views'] ?? '') === '{"errors::403":1,"layouts.nav":1}',
        'the count map is unchanged, so an older reader still renders the section',
    );

    fwrite(STDOUT, sprintf("\n%d failure(s)\n", $failures));
    exit($failures === 0 ? 0 : 1);
}
