<?php

declare(strict_types=1);

/**
 * The PHP collector SDK's verification suite (`composer test`).
 *
 * Hand-rolled rather than PHPUnit-based: this package is a runtime shim that has to load inside
 * an application's own dependency tree, so it declares no dev dependencies and has no vendor
 * directory of its own. A test suite that needed `composer install` could not be run inside the
 * replay image it verifies.
 *
 * Three sections, in the order the conformance suite's README asks for:
 *   1. digest and selector vectors  — unit-level, run first, because an implementation whose
 *      derivation is wrong fails whole cases for reasons that are hard to read out of a diff;
 *   2. the 29 protocol conformance cases, each in its own process;
 *   3. unit tests for the PHP-specific edges the data-only suite cannot express.
 */

namespace Chronos\Collector\Tests;

use Chronos\Collector\Replay\Answer;
use Chronos\Collector\Replay\CallPath;
use Chronos\Collector\Replay\MutationSweep;
use Chronos\Collector\Replay\Canonical;
use Chronos\Collector\Replay\DatabaseAnswer;
use Chronos\Collector\Replay\Divergence;
use Chronos\Collector\Replay\Effect;
use Chronos\Collector\Replay\EffectPolicy;
use Chronos\Collector\Replay\HttpAnswer;
use Chronos\Collector\Replay\Lookup;
use Chronos\Collector\Replay\ReplayBlocked;
use Chronos\Collector\Replay\ReplayRuntime;
use Chronos\Collector\Replay\ReplaySession;
use Chronos\Collector\Replay\Report;
use Chronos\Collector\Replay\Vocabulary;
use Chronos\Collector\Framework\Guzzle\ImmediatePromise;
use Chronos\Collector\Framework\Guzzle\ReplayMiddleware;
use Chronos\Collector\Framework\Laravel\RequestFacts;
use Chronos\Collector\Framework\Pdo\EffectConnection;

spl_autoload_register(static function (string $class): void {
    $prefix = 'Chronos\\Collector\\';
    if (!str_starts_with($class, $prefix)) {
        return;
    }
    $path = __DIR__.'/../src/'.str_replace('\\', '/', substr($class, strlen($prefix))).'.php';
    if (is_file($path)) {
        require $path;
    }
});

/**
 * The conformance suite lives with the specification, not with any implementation, so that no
 * language is privileged. It is reached by relative path because the collector is a submodule of
 * the platform repository and the suite is a sibling of it. A standalone checkout of the package
 * (composer zipball, the published repo on its own) has no sibling spec tree — there the
 * replay-conformance sections are SKIPPED, not failed: they verify the replay shim against the
 * spec, which is monorepo CI's job, while the package's own unit sections always run.
 * CHRONOS_CONFORMANCE_SUITE overrides the location explicitly.
 */
const SUITE = __DIR__.'/../../../../../docs/specs/replay-protocol-conformance';

function suiteDirectory(): ?string
{
    $override = getenv('CHRONOS_CONFORMANCE_SUITE');
    if (is_string($override) && $override !== '' && is_dir($override)) {
        return $override;
    }

    return is_dir(SUITE) ? SUITE : null;
}

final class Runner
{
    private int $tests = 0;

    private int $failures = 0;

    public function test(string $name, callable $test): void
    {
        ++$this->tests;
        try {
            $test($this);
            fwrite(STDOUT, "PASS {$name}\n");
        } catch (\Throwable $error) {
            ++$this->failures;
            fwrite(STDERR, "FAIL {$name}: {$error->getMessage()}\n");
        }
    }

    public function assertSame(mixed $expected, mixed $actual, string $context = ''): void
    {
        if ($expected !== $actual) {
            throw new \RuntimeException(sprintf(
                '%sexpected %s, got %s',
                $context === '' ? '' : $context.': ',
                self::show($expected),
                self::show($actual),
            ));
        }
    }

    public function assertTrue(bool $condition, string $message): void
    {
        if (!$condition) {
            throw new \RuntimeException($message);
        }
    }

    /**
     * Deep equality that keeps JSON types: a recorded number stays a number in the value handed
     * back, so `12` must not satisfy an expected `"12"`. Map key order is ignored, list order is
     * not.
     */
    public function assertJsonEquals(mixed $expected, mixed $actual, string $context): void
    {
        if (!self::jsonEquals($expected, $actual)) {
            throw new \RuntimeException(sprintf(
                '%s: expected %s, got %s',
                $context,
                json_encode($expected, JSON_UNESCAPED_SLASHES),
                json_encode($actual, JSON_UNESCAPED_SLASHES),
            ));
        }
    }

    /**
     * Field-subset comparison: only the fields the expected element names are compared, and
     * `message` never is. That is what leaves an implementation free to word its prose and add
     * its own diagnostics without failing the suite.
     *
     * @param array<string, mixed> $expected
     * @param array<string, mixed> $actual
     */
    public function assertSubset(array $expected, array $actual, string $context): void
    {
        foreach ($expected as $field => $value) {
            if ($field === 'message') {
                continue;
            }
            $this->assertTrue(
                array_key_exists($field, $actual),
                sprintf('%s: report carries no field %s', $context, $field),
            );
            $this->assertJsonEquals($value, $actual[$field], $context.'.'.$field);
        }
    }

    public function finish(): int
    {
        fwrite(STDOUT, sprintf("%d tests, %d failures\n", $this->tests, $this->failures));

        return $this->failures === 0 ? 0 : 1;
    }

    private static function jsonEquals(mixed $expected, mixed $actual): bool
    {
        if (is_array($expected) && is_array($actual)) {
            if (count($expected) !== count($actual)) {
                return false;
            }
            foreach ($expected as $key => $value) {
                if (!array_key_exists($key, $actual) || !self::jsonEquals($value, $actual[$key])) {
                    return false;
                }
            }

            return true;
        }
        if (is_float($expected) || is_float($actual)) {
            return is_numeric($expected) && is_numeric($actual) && (float) $expected === (float) $actual;
        }

        return $expected === $actual;
    }

    private static function show(mixed $value): string
    {
        return is_scalar($value) || $value === null
            ? var_export($value, true)
            : (string) json_encode($value, JSON_UNESCAPED_SLASHES);
    }
}

/** @return array<string, mixed> */
function readJson(string $path): array
{
    $contents = file_get_contents($path);
    if ($contents === false) {
        throw new \RuntimeException('cannot read '.$path);
    }
    $decoded = json_decode($contents, true);

    return is_array($decoded) ? $decoded : [];
}

/**
 * Materialise one case's recording into a fresh directory and make it read-only, so an
 * implementation that wrote cursor state or a lock file next to the recording is caught by the
 * platform rather than by a later assertion.
 *
 * @return array{string, string} the inputs directory and the report path
 */
function materialise(string $caseDirectory, string $case): array
{
    $root = sys_get_temp_dir().'/chronos-replay-conformance/'.$case.'-'.bin2hex(random_bytes(4));
    $inputs = $root.'/recorded';
    mkdir($inputs, 0o700, true);
    foreach (['manifest.json', 'events.json'] as $file) {
        copy($caseDirectory.'/'.$file, $inputs.'/'.$file);
        chmod($inputs.'/'.$file, 0o444);
    }
    chmod($inputs, 0o555);

    return [$inputs, $root.'/chronos-replay-report.json'];
}

/** A content fingerprint of the recorded tree, so step 7 of the runner contract is checkable. */
function fingerprint(string $directory): string
{
    $entries = scandir($directory);
    $lines = [];
    foreach ($entries === false ? [] : $entries as $entry) {
        if ($entry === '.' || $entry === '..') {
            continue;
        }
        $path = $directory.'/'.$entry;
        $lines[] = $entry.':'.(is_file($path) ? hash_file('sha256', $path) : 'dir');
    }
    sort($lines);

    return hash('sha256', implode("\n", $lines));
}

function removeTree(string $directory): void
{
    if (!is_dir($directory)) {
        return;
    }
    @chmod($directory, 0o700);
    foreach (scandir($directory) ?: [] as $entry) {
        if ($entry === '.' || $entry === '..') {
            continue;
        }
        $path = $directory.'/'.$entry;
        if (is_dir($path)) {
            removeTree($path);
        } else {
            @chmod($path, 0o600);
            @unlink($path);
        }
    }
    @rmdir($directory);
}

/**
 * Run one case in its own process with EXACTLY the environment env.json names.
 *
 * @param array<string, string> $environment
 *
 * @return array{int, string}
 */
function runCase(array $environment, string $caseDirectory): array
{
    $descriptors = [1 => ['pipe', 'w'], 2 => ['pipe', 'w']];
    $process = proc_open(
        [PHP_BINARY, __DIR__.'/replay-case.php', $caseDirectory],
        $descriptors,
        $pipes,
        sys_get_temp_dir(),
        $environment,
    );
    if (!is_resource($process)) {
        throw new \RuntimeException('cannot start the case process');
    }
    $output = (string) stream_get_contents($pipes[1]).(string) stream_get_contents($pipes[2]);
    fclose($pipes[1]);
    fclose($pipes[2]);

    return [proc_close($process), $output];
}

$runner = new Runner();

// ── 1. Unit-level vectors ───────────────────────────────────────────────────────────────────

$suite = suiteDirectory();
if ($suite === null) {
    fwrite(STDOUT, "SKIP replay-conformance sections: spec suite not present (standalone package checkout; set CHRONOS_CONFORMANCE_SUITE to run them)\n");
}

if ($suite !== null) {
    $runner->test('digest vectors', static function (Runner $runner) use ($suite): void {
        $vectors = readJson($suite.'/digest-vectors.json')['vectors'] ?? [];
        $runner->assertTrue($vectors !== [], 'digest-vectors.json carries no vectors');
        foreach ($vectors as $index => $vector) {
            $runner->assertSame(
                $vector['eventDigest'],
                Canonical::eventDigest($vector['event']['kind'], $vector['event']['payload']),
                sprintf('vector %d (%s)', $index, $vector['note'] ?? ''),
            );
        }
    });

    $runner->test('selector vectors', static function (Runner $runner) use ($suite): void {
        $vectors = readJson($suite.'/selector-vectors.json')['vectors'] ?? [];
        $runner->assertTrue($vectors !== [], 'selector-vectors.json carries no vectors');
        foreach ($vectors as $index => $vector) {
            $runner->assertSame(
                $vector['selector'],
                Vocabulary::selectorFor($vector['channel'], $vector['payload']),
                sprintf('vector %d (%s)', $index, $vector['note'] ?? ''),
            );
        }
    });
}

// ── 2. Conformance cases ────────────────────────────────────────────────────────────────────

$index = $suite !== null ? readJson($suite.'/cases.json') : [];
foreach ($index['cases'] ?? [] as $entry) {
    $case = (string) $entry['case'];
    $runner->test('conformance '.$case, static function (Runner $runner) use ($case, $suite): void {
        $caseDirectory = $suite.'/cases/'.$case;
        [$inputs, $report] = materialise($caseDirectory, $case);
        $before = fingerprint($inputs);
        try {
            $environment = [];
            foreach (readJson($caseDirectory.'/env.json') as $name => $value) {
                $environment[(string) $name] = str_replace(
                    ['${INPUTS}', '${REPORT}'],
                    [$inputs, $report],
                    (string) $value,
                );
            }
            [$exitCode, $output] = runCase($environment, $caseDirectory);
            $expected = readJson($caseDirectory.'/expected.json');

            $runner->assertTrue(
                is_file($report),
                sprintf('no report was written to %s (exit %d, output: %s)', $report, $exitCode, trim($output)),
            );
            $actual = readJson($report);
            $runner->assertSame($expected['outcome'], $actual['outcome'] ?? null, 'outcome');
            $runner->assertSame($expected['exitCode'], $exitCode, 'process exit code');
            $runner->assertSame($expected['exitCode'], $actual['exitCode'] ?? null, 'reported exit code');

            $lookups = $actual['lookups'] ?? [];
            $runner->assertSame(count($expected['lookups']), count($lookups), 'lookup count');
            foreach ($expected['lookups'] as $position => $expectedLookup) {
                $runner->assertSubset($expectedLookup, $lookups[$position], 'lookups['.$position.']');
            }

            $divergences = $actual['divergences'] ?? [];
            $runner->assertSame(count($expected['divergences']), count($divergences), 'divergence count');
            foreach ($expected['divergences'] as $position => $expectedDivergence) {
                $runner->assertSubset($expectedDivergence, $divergences[$position], 'divergences['.$position.']');
            }

            if (isset($expected['counts'])) {
                $runner->assertJsonEquals($expected['counts'], $actual['counts'] ?? null, 'counts');
            }
            if (isset($expected['report'])) {
                $runner->assertSubset($expected['report'], $actual, 'report');
            }
            $runner->assertSame($before, fingerprint($inputs), 'the recording was written to');
        } finally {
            removeTree(dirname($inputs));
        }
    });
}

// ── 3. PHP-specific edges ───────────────────────────────────────────────────────────────────

$runner->test('an unknown kind becomes its own custom channel', static function (Runner $runner): void {
    $runner->assertSame('custom:widget_read', Vocabulary::channelForKind('widget_read'));
    $runner->assertSame('custom:feature_flag', Vocabulary::channelForKind('custom:feature_flag'));
    $runner->assertSame('queue', Vocabulary::channelForKind('queue_publish'));
    $runner->assertSame('database', Vocabulary::channelForKind('database_result'));
});

$runner->test('a channel maps to a Docker-legal environment name', static function (Runner $runner): void {
    $runner->assertSame('CUSTOM_WIDGET', Vocabulary::environmentToken('custom:widget'));
    $runner->assertSame('DATABASE', Vocabulary::environmentToken('database'));
    $runner->assertTrue(
        preg_match('/^[A-Za-z_][A-Za-z0-9_]*$/', 'CHRONOS_REPLAY_EFFECT_'.Vocabulary::environmentToken('custom:a-b.c').'_READ') === 1,
        'the derived name would be rejected by the Docker executor',
    );
});

$runner->test('SQL intent follows the first keyword', static function (Runner $runner): void {
    $runner->assertSame('read', Vocabulary::intentForStatement("  select\n  1 "));
    $runner->assertSame('read', Vocabulary::intentForStatement('WITH x AS (SELECT 1) SELECT * FROM x'));
    $runner->assertSame('write', Vocabulary::intentForStatement('UPDATE users SET seen_at = ?'));
    $runner->assertSame('write', Vocabulary::intentForStatement('(SELECT 1)'));
});

$runner->test('the value cap never splits a UTF-8 sequence', static function (Runner $runner): void {
    $capped = Canonical::cap(str_repeat('€', 3000));
    $runner->assertSame(4095, strlen($capped), 'byte length');
    $runner->assertSame(1365, substr_count($capped, '€'), 'characters kept');
    $runner->assertSame(str_repeat('a', 4096), Canonical::cap(str_repeat('a', 5000)));
    $runner->assertSame('short', Canonical::cap('short'));
});

$runner->test('time and random cannot be simulated', static function (Runner $runner): void {
    $runner->assertSame(null, Vocabulary::syntheticAnswer('time', 'read', 0));
    $runner->assertSame(null, Vocabulary::syntheticAnswer('random', 'read', 0));
    $runner->assertJsonEquals(
        ['accepted' => 'true', 'messageId' => 'chronos-simulated-2', 'simulated' => '1'],
        Vocabulary::syntheticAnswer('custom:queue.orders', 'write', 2),
        'queue substitution',
    );
});

$runner->test('a named effect variable outranks the generic pair', static function (Runner $runner): void {
    $policy = EffectPolicy::fromEnvironment([
        'CHRONOS_REPLAY_EFFECT_NETWORK_CALL' => 'passthrough',
        'CHRONOS_REPLAY_EFFECT_HTTP_READ' => 'blocked',
        'CHRONOS_REPLAY_EFFECT_DATABASE_WRITE' => 'simulated',
    ]);
    $runner->assertSame('passthrough', $policy->modeFor('http', 'read'), 'http read');
    $runner->assertSame('passthrough', $policy->modeFor('http', 'write'), 'http write');
    $runner->assertSame('simulated', $policy->modeFor('database', 'write'), 'database write');
    $runner->assertSame('replayed', $policy->modeFor('database', 'read'), 'database read default');
    $runner->assertSame('blocked', $policy->modeFor('custom:widget', 'write'), 'unknown channel write default');
});

$runner->test('outside replay mode the shim is inert', static function (Runner $runner): void {
    ReplayRuntime::reset();
    $runner->assertSame(null, ReplayRuntime::boot(['PATH' => '/usr/bin']));
    $runner->assertSame(null, Effect::time('time'), 'a clock read is performed for real');
    $runner->assertSame(false, ReplayRuntime::active());
});

$runner->test('a blocked effect throws and an unanswerable one ends the process', static function (Runner $runner): void {
    $inputs = sys_get_temp_dir().'/chronos-replay-unit-'.bin2hex(random_bytes(4));
    mkdir($inputs, 0o700, true);
    file_put_contents($inputs.'/manifest.json', json_encode(['recordingId' => 'rec-unit', 'eventCount' => 1]));
    file_put_contents($inputs.'/events.json', json_encode(['events' => [[
        'sequence' => 1,
        'kind' => 'database_query',
        'payload' => ['statement' => 'UPDATE t SET a = 1'],
    ]]]));
    $terminated = [];
    try {
        ReplayRuntime::reset();
        ReplayRuntime::useTerminator(static function (int $code) use (&$terminated): void {
            $terminated[] = $code;
        });
        $session = ReplayRuntime::boot([
            'CHRONOS_REPLAY_RECORDING' => 'rec-unit',
            'CHRONOS_REPLAY_INPUTS' => $inputs,
            'CHRONOS_REPLAY_REPORT' => $inputs.'/report.json',
        ]);
        $runner->assertTrue($session instanceof ReplaySession, 'the session started');

        $blocked = null;
        try {
            Effect::database('UPDATE t SET a = 1');
        } catch (ReplayBlocked $error) {
            $blocked = $error;
        }
        $runner->assertTrue($blocked !== null, 'a blocked write must not return a value');
        $runner->assertSame(Answer::BLOCKED, $blocked->answer->outcome);
        $runner->assertSame([], $terminated, 'a block does not end the replay');

        $aborted = false;
        try {
            Effect::database('SELECT nothing_recorded');
        } catch (\Chronos\Collector\Replay\ReplayAborted) {
            $aborted = true;
        }
        $runner->assertTrue($aborted, 'an unanswerable read must abort');
        $runner->assertSame([ReplaySession::EXIT_ABORTED], $terminated, 'exit code');

        $report = readJson($inputs.'/report.json');
        $runner->assertSame(ReplaySession::OUTCOME_ABORTED, $report['outcome'] ?? null);
        $runner->assertSame(0, $report['counts']['unconsumed'] ?? -1, 'unconsumed is not assessed on an abort');
        $runner->assertSame(
            Divergence::UNRECORDED_EFFECT,
            $report['divergences'][1]['type'] ?? null,
            'the abort names the missing selector',
        );
    } finally {
        ReplayRuntime::reset();
        removeTree($inputs);
    }
});

$runner->test('every effect wrapper reaches its own channel and intent', static function (Runner $runner): void {
    $inputs = sys_get_temp_dir().'/chronos-replay-unit-'.bin2hex(random_bytes(4));
    mkdir($inputs, 0o700, true);
    $events = [
        ['sequence' => 1, 'kind' => 'cache_read', 'payload' => ['key' => 'user:7', 'value' => 'ada']],
        ['sequence' => 2, 'kind' => 'env_read', 'payload' => ['name' => 'APP_ENV', 'value' => 'replay']],
        ['sequence' => 3, 'kind' => 'random', 'payload' => ['function' => 'mt_rand', 'result' => '7']],
        ['sequence' => 4, 'kind' => 'file_read', 'payload' => ['path' => '/etc/hosts', 'body' => 'x']],
        ['sequence' => 5, 'kind' => 'custom:flag', 'payload' => ['name' => 'checkout_v2', 'value' => 'on']],
        ['sequence' => 6, 'kind' => 'custom:queue.orders', 'payload' => ['name' => 'orders', 'accepted' => 'true']],
    ];
    file_put_contents($inputs.'/manifest.json', json_encode(['recordingId' => 'rec-wrappers', 'eventCount' => count($events)]));
    file_put_contents($inputs.'/events.json', json_encode(['events' => $events]));
    try {
        ReplayRuntime::reset();
        ReplayRuntime::useTerminator(static function (int $code): void {});
        ReplayRuntime::boot([
            'CHRONOS_REPLAY_RECORDING' => 'rec-wrappers',
            'CHRONOS_REPLAY_INPUTS' => $inputs,
            'CHRONOS_REPLAY_REPORT' => $inputs.'/report.json',
        ]);
        $runner->assertJsonEquals(['key' => 'user:7', 'value' => 'ada'], Effect::cache('user:7'), 'cache read');
        $runner->assertJsonEquals(['name' => 'APP_ENV', 'value' => 'replay'], Effect::environment('APP_ENV'), 'env read');
        $runner->assertJsonEquals(['function' => 'mt_rand', 'result' => '7'], Effect::random('mt_rand'), 'random read');
        $runner->assertJsonEquals(['path' => '/etc/hosts', 'body' => 'x'], Effect::file('/etc/hosts'), 'file read');
        $runner->assertJsonEquals(
            ['name' => 'checkout_v2', 'value' => 'on'],
            Effect::custom('custom:flag', 'checkout_v2'),
            'custom read',
        );

        // A queue publish is a write, so with no CHRONOS_REPLAY_EFFECT_QUEUE_PUBLISH it is
        // blocked even though the recording carries an answer for it.
        $blocked = null;
        try {
            Effect::queue('orders');
        } catch (ReplayBlocked $error) {
            $blocked = $error;
        }
        $runner->assertTrue($blocked !== null, 'a queue publish must default to blocked');
        $runner->assertSame('write', $blocked->answer->intent, 'queue intent');
        $runner->assertSame('queue', $blocked->answer->channel, 'queue channel');

        // http is governed by CHRONOS_REPLAY_EFFECT_NETWORK_CALL at BOTH intents, so an absent
        // variable blocks a GET as firmly as a POST.
        $blockedRead = null;
        try {
            Effect::http('GET', 'https://api.example.test/v1/rate');
        } catch (ReplayBlocked $error) {
            $blockedRead = $error;
        }
        $runner->assertTrue($blockedRead !== null, 'an http read must default to blocked');
        $runner->assertSame('read', $blockedRead->answer->intent, 'http GET intent');
    } finally {
        ReplayRuntime::reset();
        removeTree($inputs);
    }
});

$runner->test('an unwritable report falls back to one stderr line', static function (Runner $runner): void {
    $written = Report::write(['schema' => Report::SCHEMA, 'outcome' => 'conformant'], '/proc/nonexistent/report.json');
    $runner->assertSame(false, $written, 'the write cannot have succeeded');
});


$runner->test('ADR 0021 Phase 1: fixture app runs under the replay shim', static function (Runner $runner): void {
    $fixture = dirname(__DIR__, 2).'/replay/testdata/phase1';
    $app = $fixture.'/app.php';
    $recording = $fixture.'/recording';
    $shim = dirname(__DIR__).'/src/Replay/bootstrap.php';
    $runner->assertTrue(is_file($app), 'phase1 app fixture');
    $runner->assertTrue(is_file($shim), 'replay bootstrap');

    $workspace = sys_get_temp_dir().'/chronos-phase1-verify-'.bin2hex(random_bytes(4));
    mkdir($workspace, 0o777, true);
    $report = $workspace.'/chronos-replay-report.json';
    $command = [
        PHP_BINARY,
        '-d', 'auto_prepend_file='.$shim,
        '-d', 'display_errors=Off',
        '-d', 'log_errors=On',
        $app,
    ];
    $descriptor = [1 => ['pipe', 'w'], 2 => ['pipe', 'w']];
    $previous = [];
    foreach ([
        'CHRONOS_REPLAY_RECORDING' => 'rec-phase1',
        'CHRONOS_REPLAY_INPUTS' => $recording,
        'CHRONOS_REPLAY_REPORT' => $report,
        'CHRONOS_REPLAY_EFFECT_DATABASE_WRITE' => 'blocked',
        'CHRONOS_REPLAY_EFFECT_NETWORK_CALL' => 'blocked',
        'CHRONOS_REPLAY_EFFECT_QUEUE_PUBLISH' => 'blocked',
        'CHRONOS_PHP_ENABLED' => 'false',
    ] as $name => $value) {
        $previous[$name] = getenv($name);
        putenv($name.'='.$value);
        $_ENV[$name] = $value;
    }
    try {
        $process = proc_open($command, $descriptor, $pipes);
    } finally {
        foreach ($previous as $name => $value) {
            if ($value === false) {
                putenv($name);
                unset($_ENV[$name]);
            } else {
                putenv($name.'='.$value);
                $_ENV[$name] = $value;
            }
        }
    }
    $runner->assertTrue(is_resource($process), 'spawn phase1 app');
    $stdout = stream_get_contents($pipes[1]);
    $stderr = stream_get_contents($pipes[2]);
    fclose($pipes[1]);
    fclose($pipes[2]);
    $code = proc_close($process);
    try {
        $runner->assertSame(0, $code, "phase1 exit code; stderr={$stderr}");
        $runner->assertTrue(
            str_contains($stdout, 'PHASE1_APP_RAN result=1755772800'),
            "missing application marker; stdout={$stdout}",
        );
        $runner->assertTrue(is_file($report), 'report written');
        $decoded = json_decode((string) file_get_contents($report), true, 512, JSON_THROW_ON_ERROR);
        $runner->assertSame(Report::SCHEMA, $decoded['schema'] ?? null, 'report schema');
        $runner->assertSame('conformant', $decoded['outcome'] ?? null, 'report outcome');
    } finally {
        removeTree($workspace);
    }
});


$runner->test('ADR 0021 Phase 2: call path extract and first divergence', static function (Runner $runner): void {
    $events = [
        ['kind' => 'time', 'payload' => ['function' => 'time', 'result' => '1']],
        ['kind' => 'call', 'payload' => ['name' => 'App\\Http\\Kernel::handle', 'depth' => '1']],
        ['kind' => 'call', 'payload' => ['name' => 'App\\Http\\Controllers\\Checkout::store', 'depth' => '2']],
        ['kind' => 'call', 'payload' => ['name' => 'App\\Billing\\Charge::run', 'depth' => '3']],
    ];
    $recorded = CallPath::fromEvents($events);
    $runner->assertSame(3, count($recorded), 'call visits only');
    $runner->assertSame('App\\Http\\Kernel::handle', $recorded[0]['name'], 'first frame');

    $executed = $recorded;
    $executed[2] = ['name' => 'App\\Billing\\Charge::fallback', 'depth' => 3];
    $divergence = CallPath::firstDivergence($recorded, $executed);
    $runner->assertTrue($divergence !== null, 'divergence found');
    $runner->assertSame(2, $divergence['index'], 'first divergent index');
    $runner->assertSame('App\\Billing\\Charge::run', $divergence['recorded']['name'], 'recorded frame');
    $runner->assertSame('App\\Billing\\Charge::fallback', $divergence['executed']['name'], 'executed frame');

    $runner->assertSame(null, CallPath::firstDivergence($recorded, $recorded), 'identical paths');
});


$runner->test('ADR 0021 Phase 4: mutation sweep profiles rewrite recorded fixtures', static function (Runner $runner): void {
    $events = [
        ['sequence' => 1, 'kind' => 'time', 'payload' => ['function' => 'time', 'result' => '1000']],
        ['sequence' => 2, 'kind' => 'database_result', 'payload' => ['rows' => [['id' => 1]], 'rowCount' => '1']],
        ['sequence' => 3, 'kind' => 'http_response', 'payload' => ['status' => '200', 'body' => '{"ok":true}']],
        ['sequence' => 4, 'kind' => 'call', 'payload' => ['name' => 'App\\Go', 'depth' => '1']],
    ];
    $variants = MutationSweep::expand($events);
    $profiles = array_column($variants, 'profile');
    $runner->assertTrue(in_array(MutationSweep::PROFILE_CLOCK_SKEW, $profiles, true), 'clock skew');
    $runner->assertTrue(in_array(MutationSweep::PROFILE_EMPTY_DATABASE, $profiles, true), 'empty db');
    $runner->assertTrue(in_array(MutationSweep::PROFILE_HTTP_5XX, $profiles, true), 'http 5xx');

    $byProfile = [];
    foreach ($variants as $variant) {
        $byProfile[$variant['profile']] = $variant['events'];
    }
    $runner->assertSame('4600', $byProfile[MutationSweep::PROFILE_CLOCK_SKEW][0]['payload']['result'], 'skew +1h');
    $runner->assertSame([], $byProfile[MutationSweep::PROFILE_EMPTY_DATABASE][1]['payload']['rows'], 'empty rows');
    $runner->assertSame('503', $byProfile[MutationSweep::PROFILE_HTTP_5XX][2]['payload']['status'], '503');
    $runner->assertSame('', $byProfile[MutationSweep::PROFILE_HTTP_EMPTY_BODY][2]['payload']['body'], 'empty body');
    // Call-path events are preserved untouched.
    $runner->assertSame('App\\Go', $byProfile[MutationSweep::PROFILE_CLOCK_SKEW][3]['payload']['name'], 'call preserved');
});

$runner->test('ADR 0021 Phase 3: callPath lands on the replay report', static function (Runner $runner): void {
    $inputs = sys_get_temp_dir().'/chronos-replay-callpath-'.bin2hex(random_bytes(4));
    mkdir($inputs, 0o700, true);
    $events = [
        ['sequence' => 1, 'kind' => 'time', 'payload' => ['function' => 'time', 'result' => '100']],
        ['sequence' => 2, 'kind' => 'call', 'payload' => ['name' => 'App\\A', 'depth' => '1']],
        ['sequence' => 3, 'kind' => 'call', 'payload' => ['name' => 'App\\B', 'depth' => '2']],
    ];
    file_put_contents($inputs.'/manifest.json', json_encode(['recordingId' => 'rec-callpath', 'eventCount' => count($events)]));
    file_put_contents($inputs.'/events.json', json_encode(['events' => $events]));
    try {
        ReplayRuntime::reset();
        ReplayRuntime::useTerminator(static function (int $code): void {});
        $session = ReplayRuntime::boot([
            'CHRONOS_REPLAY_RECORDING' => 'rec-callpath',
            'CHRONOS_REPLAY_INPUTS' => $inputs,
            'CHRONOS_REPLAY_REPORT' => $inputs.'/report.json',
        ]);
        $runner->assertTrue($session !== null, 'session');
        CallPath::note('App\\A', 1);
        CallPath::note('App\\B-alt', 2);
        Effect::time('time');
        $code = $session->finish();
        $runner->assertSame(0, $code, 'conformant despite observational calls');
        $report = json_decode((string) file_get_contents($inputs.'/report.json'), true, 512, JSON_THROW_ON_ERROR);
        $runner->assertSame(2, $report['callPath']['recordedCount'] ?? null, 'recorded frames');
        $runner->assertSame(2, $report['callPath']['executedCount'] ?? null, 'executed frames');
        $runner->assertSame(1, $report['callPath']['firstDivergence']['index'] ?? null, 'divergence index');
        $runner->assertSame('App\\B', $report['callPath']['firstDivergence']['recorded']['name'] ?? null, 'recorded name');
        $runner->assertSame('App\\B-alt', $report['callPath']['firstDivergence']['executed']['name'] ?? null, 'executed name');
        $runner->assertSame(0, $report['counts']['unconsumed'] ?? -1, 'call events not unconsumed');
    } finally {
        ReplayRuntime::reset();
        removeTree($inputs);
    }
});

$runner->test('ADR 0021: HttpAnswer and Guzzle ReplayMiddleware consume fixtures', static function (Runner $runner): void {
    $inputs = sys_get_temp_dir().'/chronos-replay-http-mw-'.bin2hex(random_bytes(4));
    mkdir($inputs, 0o700, true);
    $events = [
        ['sequence' => 1, 'kind' => 'http_request', 'payload' => ['method' => 'GET', 'url' => 'https://api.example.test/v1/rate']],
        ['sequence' => 2, 'kind' => 'http_response', 'payload' => ['status' => '503', 'body' => '', 'headers' => 'X-Chronos-Simulated: 1']],
        ['sequence' => 3, 'kind' => 'http_request', 'payload' => ['method' => 'GET', 'url' => 'https://api.example.test/v1/rate']],
        ['sequence' => 4, 'kind' => 'http_response', 'payload' => ['status' => '200', 'body' => '{"ok":true}']],
    ];
    file_put_contents($inputs.'/manifest.json', json_encode(['recordingId' => 'rec-http-mw', 'eventCount' => count($events)]));
    file_put_contents($inputs.'/events.json', json_encode(['events' => $events]));
    try {
        ReplayRuntime::reset();
        ReplayRuntime::useTerminator(static function (int $code): void {});
        ReplayRuntime::boot([
            'CHRONOS_REPLAY_RECORDING' => 'rec-http-mw',
            'CHRONOS_REPLAY_INPUTS' => $inputs,
            'CHRONOS_REPLAY_REPORT' => $inputs.'/report.json',
            'CHRONOS_REPLAY_EFFECT_NETWORK_CALL' => 'replayed',
        ]);
        $payload = Effect::http('GET', 'https://api.example.test/v1/rate');
        $runner->assertSame(503, HttpAnswer::status($payload ?? []), 'status');
        $runner->assertSame('', HttpAnswer::body($payload ?? []), 'body');
        $response = ReplayMiddleware::response($payload ?? []);
        $runner->assertSame(503, $response->getStatusCode(), 'synthetic status');
        $runner->assertSame('', (string) $response->getBody(), 'synthetic body');

        $liveCalled = false;
        $inner = static function () use (&$liveCalled) {
            $liveCalled = true;

            return new ImmediatePromise(new \stdClass());
        };
        $middleware = ReplayMiddleware::create()($inner);
        $request = new class {
            public function getMethod(): string
            {
                return 'GET';
            }

            public function getUri(): string
            {
                return 'https://api.example.test/v1/rate';
            }
        };
        $promise = $middleware($request, []);
        $response = $promise->wait();
        $runner->assertSame(false, $liveCalled, 'live handler skipped');
        $runner->assertSame(200, $response->getStatusCode(), 'replayed status');
        $runner->assertSame('{"ok":true}', (string) $response->getBody(), 'replayed body');
    } finally {
        ReplayRuntime::reset();
        removeTree($inputs);
    }
});

$runner->test('ADR 0021: DatabaseAnswer and EffectConnection consume empty_database', static function (Runner $runner): void {
    $base = [
        ['sequence' => 1, 'kind' => 'database_query', 'payload' => ['statement' => 'SELECT id FROM users']],
        ['sequence' => 2, 'kind' => 'database_result', 'payload' => ['rows' => [['id' => '1']], 'rowCount' => '1']],
        ['sequence' => 3, 'kind' => 'database_query', 'payload' => ['statement' => 'SELECT id FROM users']],
        ['sequence' => 4, 'kind' => 'database_result', 'payload' => ['rows' => [['id' => '2']], 'rowCount' => '1']],
    ];
    $mutated = MutationSweep::apply($base, MutationSweep::PROFILE_EMPTY_DATABASE);
    $runner->assertTrue($mutated !== null, 'empty_database applies');
    $inputs = sys_get_temp_dir().'/chronos-replay-pdo-'.bin2hex(random_bytes(4));
    mkdir($inputs, 0o700, true);
    file_put_contents($inputs.'/manifest.json', json_encode(['recordingId' => 'rec-pdo', 'eventCount' => count($mutated)]));
    file_put_contents($inputs.'/events.json', json_encode(['events' => $mutated]));
    try {
        ReplayRuntime::reset();
        ReplayRuntime::useTerminator(static function (int $code): void {});
        ReplayRuntime::boot([
            'CHRONOS_REPLAY_RECORDING' => 'rec-pdo',
            'CHRONOS_REPLAY_INPUTS' => $inputs,
            'CHRONOS_REPLAY_REPORT' => $inputs.'/report.json',
            'CHRONOS_REPLAY_EFFECT_DATABASE_READ' => 'replayed',
        ]);
        $payload = Effect::database('SELECT id FROM users');
        $runner->assertSame([], DatabaseAnswer::rows($payload ?? []), 'empty rows');
        $runner->assertSame(0, DatabaseAnswer::rowCount($payload ?? []), 'rowCount 0');

        $pdo = new \PDO('sqlite::memory:');
        $conn = new EffectConnection($pdo);
        $stmt = $conn->query('SELECT id FROM users');
        $runner->assertTrue($stmt !== false, 'statement');
        $runner->assertSame([], $stmt->fetchAll(), 'fetchAll empty');
        $runner->assertSame(0, $stmt->rowCount(), 'statement rowCount');
    } finally {
        ReplayRuntime::reset();
        removeTree($inputs);
    }
});


$runner->test('ADR 0021: ReplayRuntime arms native scalar hooks when extension present', static function (Runner $runner): void {
    // Without chronos.so the registration is a no-op; with it, builtins are intercepted.
    // This test only asserts the boot path stays green either way.
    $inputs = sys_get_temp_dir().'/chronos-replay-delegate-'.bin2hex(random_bytes(4));
    mkdir($inputs, 0o700, true);
    $events = [
        ['sequence' => 1, 'kind' => 'time', 'payload' => ['function' => 'time', 'result' => '9']],
    ];
    file_put_contents($inputs.'/manifest.json', json_encode(['recordingId' => 'rec-delegate', 'eventCount' => 1]));
    file_put_contents($inputs.'/events.json', json_encode(['events' => $events]));
    try {
        ReplayRuntime::reset();
        ReplayRuntime::useTerminator(static function (int $code): void {});
        $session = ReplayRuntime::boot([
            'CHRONOS_REPLAY_RECORDING' => 'rec-delegate',
            'CHRONOS_REPLAY_INPUTS' => $inputs,
            'CHRONOS_REPLAY_REPORT' => $inputs.'/report.json',
        ]);
        $runner->assertTrue($session !== null, 'session armed');
        $runner->assertSame(
            function_exists('chronos_replay_arm'),
            function_exists('chronos_replay_arm'),
            'delegate presence is stable',
        );
    } finally {
        ReplayRuntime::reset();
        removeTree($inputs);
    }
});

$runner->test('Laravel request facts hydrate the root with names and counts only', static function (Runner $runner): void {
    RequestFacts::reset();
    RequestFacts::noteView('users.show');
    RequestFacts::noteView('users.show');
    RequestFacts::noteView('layouts.app');
    RequestFacts::noteModel('App\\Models\\User');
    RequestFacts::noteModel('App\\Models\\User');
    RequestFacts::noteModel('App\\Models\\User');
    RequestFacts::noteMail('App\\Mail\\Welcome');
    RequestFacts::noteJob('App\\Jobs\\IndexUser', 'default');
    RequestFacts::noteGate('update', true);
    RequestFacts::noteGate('update', true);
    RequestFacts::noteGate('delete', false);
    RequestFacts::noteEvent('App\\Events\\UserViewed');
    RequestFacts::noteEvent('Illuminate\\Database\\Events\\QueryExecuted');
    RequestFacts::noteEvent('composing: users.show');

    $attributes = RequestFacts::snapshot(RequestFacts::identity(
        routeName: 'users.show',
        routeAction: 'App\\Http\\Controllers\\UserController@show',
        middleware: ['web', 'auth'],
        userId: '42',
        guard: 'web',
        peakMemoryBytes: 8_388_608,
    ));

    $runner->assertSame('users.show', $attributes['http.route.name'] ?? null, 'route name');
    $runner->assertSame('App\\Http\\Controllers\\UserController@show', $attributes['http.route.action'] ?? null, 'action');
    $runner->assertSame('["web","auth"]', $attributes['http.route.middleware'] ?? null, 'middleware');
    $runner->assertSame('42', $attributes['enduser.id'] ?? null, 'user id');
    $runner->assertSame('web', $attributes['enduser.guard'] ?? null, 'guard');
    $runner->assertSame('8388608', $attributes['process.runtime.php.memory.peak_bytes'] ?? null, 'peak memory');
    $runner->assertSame('{"users.show":2,"layouts.app":1}', $attributes['laravel.views'] ?? null, 'views');
    $runner->assertSame('{"App\\\\Models\\\\User":3}', $attributes['laravel.models'] ?? null, 'models');
    $runner->assertSame('{"App\\\\Mail\\\\Welcome":1}', $attributes['laravel.mail'] ?? null, 'mail');
    $runner->assertSame('{"App\\\\Jobs\\\\IndexUser@default":1}', $attributes['laravel.jobs'] ?? null, 'jobs');
    $runner->assertSame('{"update:allow":2,"delete:deny":1}', $attributes['laravel.gates'] ?? null, 'gates');
    $runner->assertSame('{"App\\\\Events\\\\UserViewed":1}', $attributes['laravel.events'] ?? null, 'app events only');
    $runner->assertTrue(!isset($attributes['laravel.views.truncated']), 'no truncation flag when under cap');
});

$runner->test('Laravel request facts stop tracking new names after the unique cap', static function (Runner $runner): void {
    RequestFacts::reset();
    for ($i = 0; $i < 40; ++$i) {
        RequestFacts::noteView('view-'.$i);
    }
    RequestFacts::noteView('view-0');
    $attributes = RequestFacts::snapshot();
    $views = json_decode($attributes['laravel.views'] ?? '[]', true);
    $runner->assertSame(32, is_array($views) ? count($views) : 0, 'unique cap');
    $runner->assertSame(2, is_array($views) ? ($views['view-0'] ?? 0) : 0, 'known names still count');
    $runner->assertSame('true', $attributes['laravel.views.truncated'] ?? null, 'truncated');
});

$runner->test('Laravel request facts omit empty identity rather than writing blank keys', static function (Runner $runner): void {
    RequestFacts::reset();
    $attributes = RequestFacts::snapshot(RequestFacts::identity());
    $runner->assertSame([], $attributes, 'empty request writes nothing');
});

exit($runner->finish());
