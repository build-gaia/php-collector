<?php

declare(strict_types=1);

/**
 * Native-extension behaviour case: the four 2026-09 additions to the Rust .so.
 *
 *   1. Prepared statements — `SQLite3Stmt::execute` (and by the same map,
 *      `mysqli_stmt::execute`) emits a client span carrying the SQL that was only
 *      ever visible at prepare time.
 *   2. Propagation — inbound tracestate/baggage are stored verbatim, capped, echoed
 *      by chronos_propagation_headers(), and forwarded on native curl injection
 *      (W3C Trace Context requires the tracestate forwarding).
 *   3. Uncaught errors without a framework — a request dying on an uncaught
 *      exception stamps error.type/error.message/error.handled=false onto its root
 *      span, and a bridge that already reported wins.
 *   4. Semconv dual-emit — the root span carries http.request.method /
 *      http.response.status_code next to the legacy spellings.
 *
 * Standalone like every test here (no PHPUnit, exit(1) on failure). The subject is
 * native code, so each case runs a REAL child `php -d extension=<dylib>` process
 * and inspects what the extension answered or spooled; when no local build of the
 * dylib exists the seam is stubbed instead (the contract shape is asserted and the
 * live sections are reported as skipped, not silently passed off as tested).
 */

namespace Chronos\Collector\Tests;

$tests = 0;
$failures = 0;

function test(string $name, callable $case): void
{
    global $tests, $failures;
    ++$tests;
    try {
        $case();
        fwrite(STDOUT, "PASS {$name}\n");
    } catch (\Throwable $error) {
        ++$failures;
        fwrite(STDERR, "FAIL {$name}: {$error->getMessage()}\n");
    }
}

function assertSame(mixed $expected, mixed $actual, string $context): void
{
    if ($expected !== $actual) {
        throw new \RuntimeException(sprintf(
            '%s: expected %s, got %s',
            $context,
            var_export($expected, true),
            var_export($actual, true),
        ));
    }
}

function assertTrue(bool $condition, string $message): void
{
    if (!$condition) {
        throw new \RuntimeException($message);
    }
}

/**
 * The locally built extension, debug preferred (it is what `cargo build` just
 * produced), release accepted. Null when neither exists — stub mode.
 */
function extensionPath(): ?string
{
    $native = \dirname(__DIR__, 2).'/native/target';
    foreach (["{$native}/debug/libchronos.dylib", "{$native}/release/libchronos.dylib",
              "{$native}/debug/libchronos.so", "{$native}/release/libchronos.so"] as $candidate) {
        if (is_file($candidate)) {
            return $candidate;
        }
    }

    return null;
}

/**
 * Run PHP code in a child process with the extension loaded and a scratch spool.
 * Returns [stdout, spool array of decoded .trace batches].
 *
 * The child's environment is scrubbed of CHRONOS_PHP_* because env beats INI in
 * the settings layer — a developer's exported collector config would silently
 * redirect the spool and fail every assertion here for the wrong reason.
 *
 * @param list<string> $extraIni extra `-d` pairs, e.g. 'chronos.cli_enabled=1'
 * @return array{0: string, 1: list<array<string, mixed>>}
 */
function runInstrumented(string $code, array $extraIni = []): array
{
    $spool = sys_get_temp_dir().'/chronos-native-case-'.bin2hex(random_bytes(6));
    mkdir($spool, 0777, true);

    $ini = array_merge([
        'chronos.enabled=1',
        'chronos.apm_enabled=1',
        'chronos.organisation=test-org',
        'chronos.project=test-team',
        'chronos.application=test-app',
        "chronos.spool_directory={$spool}",
    ], $extraIni);

    $command = [PHP_BINARY, '-d', 'extension='.extensionPath()];
    foreach ($ini as $entry) {
        $command[] = '-d';
        $command[] = $entry;
    }
    $command[] = '-r';
    $command[] = $code;

    $environment = [];
    foreach (getenv() as $key => $value) {
        if (!str_starts_with($key, 'CHRONOS_')) {
            $environment[$key] = $value;
        }
    }

    $process = proc_open($command, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes, null, $environment);
    assertTrue(is_resource($process), 'child php process failed to start');
    $stdout = stream_get_contents($pipes[1]) ?: '';
    stream_get_contents($pipes[2]); // stderr: fatals are expected in the uncaught case
    proc_close($process);

    $batches = [];
    foreach (glob("{$spool}/test-org/*.trace") ?: [] as $file) {
        $decoded = json_decode((string) file_get_contents($file), true);
        if (is_array($decoded)) {
            $batches[] = $decoded;
        }
        unlink($file);
    }
    array_map('unlink', glob("{$spool}/test-org/*") ?: []);
    @rmdir("{$spool}/test-org");
    @rmdir($spool);

    return [$stdout, $batches];
}

/** @return array<string, mixed>|null the first span named $name across batches */
function findSpan(array $batches, string $name): ?array
{
    foreach ($batches as $batch) {
        foreach ($batch['spans'] ?? [] as $span) {
            if (($span['name'] ?? '') === $name) {
                return $span;
            }
        }
    }

    return null;
}

// ---------------------------------------------------------------------------
// Stub mode: no local dylib. Assert the seam's CONTRACT so the PHP bridges'
// expectations stay pinned, and stop — a stub cannot witness native behaviour.
// ---------------------------------------------------------------------------
if (extensionPath() === null) {
    fwrite(STDOUT, "SKIP live native cases: no built dylib under native/target (run `cargo build` there)\n");

    // The seam as the bridges will consume it: three fixed keys, strings, empty
    // when absent. This stub is the reference shape for Guzzle/HttpClient/PSR-18.
    function chronos_propagation_headers(): array
    {
        return ['traceparent' => '', 'tracestate' => '', 'baggage' => ''];
    }

    test('propagation seam contract: three string keys, empty when absent', function (): void {
        $headers = \Chronos\Collector\Tests\chronos_propagation_headers();
        // Key SET, not order: the native map carries no order guarantee and the
        // bridges address it by key.
        ksort($headers);
        assertSame(['baggage' => '', 'traceparent' => '', 'tracestate' => ''], $headers, 'seam shape');
    });

    exit($GLOBALS['failures'] > 0 ? 1 : 0);
}

// ---------------------------------------------------------------------------
// 2. Propagation: chronos_propagation_headers() and the verbatim store.
// ---------------------------------------------------------------------------

test('propagation headers are all empty before any request opens', function (): void {
    [$out] = runInstrumented(
        'echo json_encode(chronos_propagation_headers());'
    );
    $headers = json_decode(trim($out), true);
    assertTrue(is_array($headers), "answer was not JSON: {$out}");
    // Key SET, not order: the native map carries no order guarantee and the
    // bridges address it by key.
    ksort($headers);
    assertSame(['baggage' => '', 'traceparent' => '', 'tracestate' => ''], $headers, 'no-request answer');
});

test('inbound tracestate and baggage are stored verbatim and the trace id continues', function (): void {
    // cli_enabled stays OFF so RINIT does not pre-open the request: this drives
    // the FULL start path, where the traceparent argument decides the trace id.
    [$out] = runInstrumented(<<<'PHP'
        chronos_request_start(
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            "vendor=state,other=x", "userId=42;tier=gold",
            "", "", "GET", "", "svc"
        );
        echo json_encode(chronos_propagation_headers());
        chronos_request_end(200, "");
        PHP);
    $headers = json_decode(trim($out), true);
    assertSame('vendor=state,other=x', $headers['tracestate'], 'tracestate is verbatim');
    assertSame('userId=42;tier=gold', $headers['baggage'], 'baggage is verbatim');
    assertTrue(
        str_starts_with($headers['traceparent'], '00-0af7651916cd43dd8448eb211c80319c-'),
        "outbound traceparent must continue the inbound trace id, got {$headers['traceparent']}",
    );
    assertTrue(
        !str_contains($headers['traceparent'], 'b7ad6b7169203331'),
        'outbound traceparent must mint a fresh span id, not replay the parent',
    );
});

test('tracestate and baggage are capped at 4096 bytes, on a char boundary', function (): void {
    $long = str_repeat('k=v,', 2000); // 8000 bytes
    [$out] = runInstrumented(
        'chronos_request_start("", '.var_export($long, true).', '.var_export($long, true).', "", "", "GET", "", "svc");'
        .'$h = chronos_propagation_headers();'
        .'echo strlen($h["tracestate"]), " ", strlen($h["baggage"]);'
        .'chronos_request_end(200, "");'
    );
    assertSame('4096 4096', trim($out), 'both headers capped at 4096 bytes');
});

test('curl forwards traceparent, tracestate and baggage, deduping but never touching the app\'s own headers', function (): void {
    // A local echo server: the child curls it and prints the headers it received.
    $port = random_int(20000, 60000);
    $router = sys_get_temp_dir().'/chronos-native-case-router-'.bin2hex(random_bytes(4)).'.php';
    file_put_contents($router, '<?php header("Content-Type: application/json"); echo json_encode(getallheaders());');
    $server = proc_open(
        [PHP_BINARY, '-S', "127.0.0.1:{$port}", $router],
        [1 => ['pipe', 'w'], 2 => ['pipe', 'w']],
        $serverPipes,
    );
    assertTrue(is_resource($server), 'echo server failed to start');
    try {
        // Wait for the server socket rather than sleeping a fixed guess.
        $ready = false;
        for ($i = 0; $i < 50 && !$ready; ++$i) {
            $socket = @fsockopen('127.0.0.1', $port, $errno, $errstr, 0.1);
            if (is_resource($socket)) {
                fclose($socket);
                $ready = true;
            } else {
                usleep(100_000);
            }
        }
        assertTrue($ready, 'echo server never came up');

        [$out] = runInstrumented(<<<PHP
            chronos_request_start(
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                "vendor=state", "userId=42", "", "", "GET", "", "svc"
            );
            \$ch = curl_init("http://127.0.0.1:{$port}/");
            curl_setopt(\$ch, CURLOPT_RETURNTRANSFER, true);
            // The app's own list, including a STALE tracestate the dedupe must evict.
            curl_setopt(\$ch, CURLOPT_HTTPHEADER, ["X-App-Own: yes", "tracestate: stale=1"]);
            echo curl_exec(\$ch);
            chronos_request_end(200, "");
            PHP);
        $received = json_decode(trim($out), true);
        assertTrue(is_array($received), "echo server answer was not JSON: {$out}");
        $received = array_change_key_case($received, CASE_LOWER);
        assertSame('yes', $received['x-app-own'] ?? null, "the application's own header survives injection");
        assertSame('vendor=state', $received['tracestate'] ?? null, 'inbound tracestate forwarded, stale one evicted');
        assertSame('userId=42', $received['baggage'] ?? null, 'inbound baggage forwarded');
        assertTrue(
            str_starts_with($received['traceparent'] ?? '', '00-0af7651916cd43dd8448eb211c80319c-'),
            'traceparent forwarded on the same trace',
        );
    } finally {
        proc_terminate($server);
        proc_close($server);
        unlink($router);
    }
});

// ---------------------------------------------------------------------------
// 1. Prepared statements: the SQL banked at prepare time reaches the execute span.
// ---------------------------------------------------------------------------

test('SQLite3Stmt::execute carries the SQL captured at prepare time (both spellings)', function (): void {
    if (!class_exists(\SQLite3::class)) {
        fwrite(STDOUT, "  (sqlite3 extension unavailable — prepared-statement case not exercised)\n");

        return;
    }
    [, $batches] = runInstrumented(<<<'PHP'
        chronos_request_start("", "", "", "", "", "GET", "", "svc");
        $db = new SQLite3(":memory:");
        $db->exec("CREATE TABLE t(x INTEGER)");
        $stmt = $db->prepare("INSERT INTO t VALUES (:x)");
        $stmt->bindValue(":x", 1, SQLITE3_INTEGER);
        $stmt->execute();
        chronos_request_end(200, "");
        PHP);
    $span = findSpan($batches, 'SQLite3Stmt::execute');
    assertTrue($span !== null, 'an SQLite3Stmt::execute span was spooled');
    assertSame('INSERT INTO t VALUES (:x)', $span['attributes']['db.query.text'] ?? null, 'current semconv key');
    assertSame('INSERT INTO t VALUES (:x)', $span['attributes']['db.statement'] ?? null, 'legacy key kept');
    assertSame('client', $span['attributes']['span.kind'] ?? null, 'client span');
});

// ---------------------------------------------------------------------------
// 3. Uncaught errors without a framework.
// ---------------------------------------------------------------------------

test('an uncaught exception stamps error.* onto the root span at shutdown', function (): void {
    [, $batches] = runInstrumented(<<<'PHP'
        chronos_request_start("", "", "", "", "", "GET", "", "svc");
        throw new RuntimeException("boom at the top level");
        PHP);
    $root = findSpan($batches, 'request');
    assertTrue($root !== null, 'the root span was spooled despite the fatal');
    assertSame('error', $root['status'] ?? null, 'root span status');
    assertSame('RuntimeException', $root['attributes']['error.type'] ?? null, 'error.type from the throw hook');
    assertSame('boom at the top level', $root['attributes']['error.message'] ?? null, 'error.message');
    assertSame('false', $root['attributes']['error.handled'] ?? null, 'unhandled by definition');
});

test('a caught exception followed by a clean end stamps nothing', function (): void {
    [, $batches] = runInstrumented(<<<'PHP'
        chronos_request_start("", "", "", "", "", "GET", "", "svc");
        try {
            throw new RuntimeException("caught and survived");
        } catch (RuntimeException) {
        }
        chronos_request_end(200, "");
        PHP);
    $root = findSpan($batches, 'request');
    assertTrue($root !== null, 'the root span was spooled');
    assertSame('ok', $root['status'] ?? null, 'a survived throw is not an errored request');
    assertTrue(!isset($root['attributes']['error.type']), 'no error.* invented for a caught exception');
});

test('a bridge that already reported wins over the native uncaught heuristic', function (): void {
    // The SDK's fatal-error shutdown net: userland shutdown functions run BEFORE
    // module RSHUTDOWN, so the bridge's report lands first and the idempotent
    // second flush no-ops — the native stamp must never overwrite it.
    [, $batches] = runInstrumented(<<<'PHP'
        chronos_request_start("", "", "", "", "", "GET", "", "svc");
        register_shutdown_function(static function (): void {
            chronos_request_end(500, "", "App\\HandlerReported", "the bridge saw it first", "", "", null, true);
        });
        throw new RuntimeException("boom the bridge will report differently");
        PHP);
    $root = findSpan($batches, 'request');
    assertTrue($root !== null, 'the root span was spooled');
    assertSame('App\\HandlerReported', $root['attributes']['error.type'] ?? null, "the bridge's identity stands");
    assertSame('true', $root['attributes']['error.handled'] ?? null, "the bridge's handled verdict stands");
});

// ---------------------------------------------------------------------------
// 4. Semconv dual-emit on the root span.
// ---------------------------------------------------------------------------

test('the root span dual-emits method and status under legacy and current keys', function (): void {
    [, $batches] = runInstrumented(<<<'PHP'
        chronos_request_start("", "", "", "", "", "POST", "", "svc");
        chronos_request_end(201, "/orders");
        PHP);
    $root = findSpan($batches, '/orders');
    assertTrue($root !== null, 'the root span was spooled under its route name');
    assertSame('POST', $root['attributes']['http.method'] ?? null, 'legacy method key kept');
    assertSame('POST', $root['attributes']['http.request.method'] ?? null, 'current method key added');
    assertSame('201', $root['attributes']['http.status_code'] ?? null, 'legacy status key kept');
    assertSame('201', $root['attributes']['http.response.status_code'] ?? null, 'current status key added');
});

if ($failures > 0) {
    fwrite(STDERR, "{$failures} of {$tests} cases failed\n");
    exit(1);
}
fwrite(STDOUT, "{$tests} cases passed\n");
