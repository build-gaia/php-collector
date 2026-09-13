<?php

declare(strict_types=1);

/**
 * Native messaging-table behaviour case: the RdKafka / NATS / php-amqplib
 * publish tables added alongside Bunny's (see observer.rs
 * MESSAGING_PUBLISH_METHODS / MESSAGING_NATS_SEND_METHOD and each broker's
 * contract in the chronos-desktop task record).
 *
 * WHY fake userland classes prove native behaviour: observe_policy matches by
 * QUALIFIED NAME, deliberately ahead of the `is_internal` early return, so a
 * userland class named `RdKafka\ProducerTopic` is observed by the exact same
 * table entry, begin handler and injection path the real C-extension class
 * would be — the one thing this cannot prove is `is_internal == true` ordering
 * itself, which is compile-time-fixed in observe_policy and covered by review.
 * What it CAN prove, in a real Zend process with the real .so loaded:
 *
 *   1. `producev` full-arity: traceparent/tracestate/enqueued-at land in the
 *      headers ARGUMENT the method body actually sees (the COW write sticks).
 *   2. `producev` short-arity — THE ARGC GUARD: a caller omitting the trailing
 *      optional `$headers` (argc 3 < index 4) must get a warn line and an
 *      unwritten slot, never a crash and never a fabricated argument. This is
 *      the RECV_INIT hazard the RdKafka contract calls "exercised in practice,
 *      not theoretical".
 *   3. `produce`: span-only by construction — no headers slot exists, nothing
 *      is written anywhere, and the call must not crash.
 *   4. NATS `Connection::sendMessage(Publish)`: the two-hop property write
 *      (`$message->payload->headers`) sticks, through the immutable-[]
 *      default the Payload constructor produces (the COMMON case), and a
 *      caller-set traceparent wins.
 *   5. NATS + `chronos_suppress_native('messaging')`: the per-request seam
 *      demotes the Publish branch — nothing injected, exactly like Bunny.
 *   6. amqplib `basic_publish`: absent `application_headers` gets a fresh
 *      plain array of `['S', value]` tuples; an existing `AMQPTable`-shaped
 *      object gets add-if-absent `[14, value]` tuples into its `$data`.
 *
 * Standalone like every test here (no PHPUnit, exit(1) on failure); each case
 * runs a REAL child `php -d extension=<dylib>` process. Skips (does not fail)
 * when no local dylib build exists, same rule as native-extension-case.php.
 */

namespace Chronos\Collector\Tests\MessagingNative;

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
 * Run PHP code in a child with the extension loaded. Returns [stdout, stderr].
 * Environment scrubbed of CHRONOS_PHP_* (env beats INI in the settings layer).
 */
function runInstrumented(string $code): array
{
    $spool = sys_get_temp_dir().'/chronos-messaging-case-'.bin2hex(random_bytes(6));
    @mkdir($spool, 0777, true);

    $command = [PHP_BINARY, '-d', 'extension='.extensionPath(),
        '-d', 'chronos.enabled=1',
        '-d', 'chronos.apm_enabled=1',
        '-d', 'chronos.organisation=test-org',
        '-d', 'chronos.project=test-team',
        '-d', 'chronos.application=test-app',
        '-d', "chronos.spool_directory={$spool}",
        '-r', $code,
    ];

    $environment = [];
    foreach (getenv() as $key => $value) {
        if (!str_starts_with($key, 'CHRONOS_')) {
            $environment[$key] = $value;
        }
    }

    $process = proc_open($command, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes, null, $environment);
    $stdout = stream_get_contents($pipes[1]);
    $stderr = stream_get_contents($pipes[2]);
    $status = proc_close($process);
    if ($status !== 0) {
        throw new \RuntimeException("child exited {$status}; stderr: {$stderr}");
    }

    return [$stdout, $stderr];
}

/** The last JSON line of a child's stdout (the extension banner precedes it on stderr). */
function lastJson(string $stdout): mixed
{
    $lines = array_values(array_filter(array_map('trim', explode("\n", $stdout))));
    return json_decode(end($lines), true);
}

if (extensionPath() === null) {
    fwrite(STDOUT, "SKIP messaging-native-case: no local libchronos build (cargo build in sdks/php/native first)\n");
    exit(0);
}

const OPEN_REQUEST = <<<'PHP'
chronos_request_start(
    "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
    "vendor=state", "tier=gold", "", "", "GET", "", "svc"
);
PHP;

const RDKAFKA_TOPIC = <<<'PHP'
namespace RdKafka;
class ProducerTopic {
    public function getName(): string { return "orders.events"; }
    public function produce(int $partition, int $msgflags, ?string $payload = null, ?string $key = null): void {
        echo json_encode(["produced" => $payload]), "\n";
    }
    public function producev(int $partition, int $msgflags, ?string $payload = null, ?string $key = null, ?array $headers = null, ?int $timestamp_ms = null, ?string $msg_opaque = null): void {
        echo json_encode($headers), "\n";
    }
}
PHP;

test('producev full-arity: the COW argument write sticks, continuing the inbound trace', function (): void {
    [$out] = runInstrumented(RDKAFKA_TOPIC."\n".OPEN_REQUEST.<<<'PHP'

$topic = new \RdKafka\ProducerTopic();
$topic->producev(0, 0, "body-bytes", "k1", []);
chronos_request_end(200, "");
PHP);
    $headers = lastJson($out);
    assertTrue(is_array($headers), 'headers array reached the method body');
    assertTrue(
        str_starts_with($headers['traceparent'] ?? '', '00-0af7651916cd43dd8448eb211c80319c-'),
        'traceparent continues the inbound trace id, got '.var_export($headers['traceparent'] ?? null, true),
    );
    assertSame('vendor=state', $headers['tracestate'] ?? null, 'tracestate forwarded verbatim');
    assertSame('tier=gold', $headers['baggage'] ?? null, 'baggage forwarded verbatim');
    assertTrue(isset($headers['x-chronos-enqueued-at']), 'enqueued-at stamp rides the publish');
});

test('producev full-arity: a caller-set traceparent wins, never rewritten', function (): void {
    [$out] = runInstrumented(RDKAFKA_TOPIC."\n".OPEN_REQUEST.<<<'PHP'

$topic = new \RdKafka\ProducerTopic();
$topic->producev(0, 0, "body", "k", ["traceparent" => "00-caller-owns-this-01"]);
chronos_request_end(200, "");
PHP);
    $headers = lastJson($out);
    assertSame('00-caller-owns-this-01', $headers['traceparent'] ?? null, 'caller traceparent kept');
    assertTrue(isset($headers['x-chronos-enqueued-at']), 'other entries still added around it');
});

test('producev short-arity: the argc guard refuses the unpassed slot — warn line, no crash, no fabricated argument', function (): void {
    [$out, $err] = runInstrumented(RDKAFKA_TOPIC."\n".OPEN_REQUEST.<<<'PHP'

$topic = new \RdKafka\ProducerTopic();
$topic->producev(0, 0, "body");
echo json_encode(["after" => true]), "\n";
chronos_request_end(200, "");
PHP);
    // The method body must see its own default (null), never a slot the
    // observer conjured: RECV_INIT against a pre-written slot is the
    // PHP-version-sensitive corruption the guard exists to prevent.
    assertTrue(str_contains($out, 'null'), 'headers stayed the declared default (null)');
    assertSame(['after' => true], lastJson($out), 'execution continued past the publish');
    assertTrue(
        str_contains($err, 'headers argument not passed'),
        "warn-once line names the reason; stderr was: {$err}",
    );
});

test('produce: span-only by construction — no headers slot exists, nothing written, no crash', function (): void {
    [$out, $err] = runInstrumented(RDKAFKA_TOPIC."\n".OPEN_REQUEST.<<<'PHP'

$topic = new \RdKafka\ProducerTopic();
$topic->produce(0, 0, "plain-body", "k");
chronos_request_end(200, "");
PHP);
    assertSame(['produced' => 'plain-body'], lastJson($out), 'payload arrived untouched');
    assertTrue(
        !str_contains($err, 'traceparent not injected'),
        'produce() never attempts injection, so it must never warn about failing it',
    );
});

const NATS_CLASSES = <<<'PHP'
namespace Basis\Nats\Message;
class Publish {
    public string $subject = "orders.created";
    public object $payload;
    public function __construct() {
        $this->payload = new \Basis\Nats\Message\Payload();
    }
}
class Payload {
    public string $body = "nats-body";
    public array $headers = [];
}

namespace Basis\Nats;
class Connection {
    public function sendMessage(object $message): void {
        echo json_encode($message->payload->headers), "\n";
    }
}
PHP;

test('NATS Publish: the two-hop property write sticks through the immutable-[] default', function (): void {
    [$out] = runInstrumented(NATS_CLASSES."\n".OPEN_REQUEST.<<<'PHP'

$connection = new \Basis\Nats\Connection();
$connection->sendMessage(new \Basis\Nats\Message\Publish());
chronos_request_end(200, "");
PHP);
    $headers = lastJson($out);
    assertTrue(is_array($headers), 'payload->headers readable in the method body');
    assertTrue(
        str_starts_with($headers['traceparent'] ?? '', '00-0af7651916cd43dd8448eb211c80319c-'),
        'traceparent landed in $message->payload->headers',
    );
    assertTrue(isset($headers['x-chronos-enqueued-at']), 'enqueued-at stamp rides the NATS publish');
});

test('NATS Publish SUBCLASS: runtime dispatch is real instanceof (parent walk), not an exact name compare', function (): void {
    [$out] = runInstrumented(NATS_CLASSES."\n".OPEN_REQUEST.<<<'PHP'

class PriorityPublish extends \Basis\Nats\Message\Publish {}
$connection = new \Basis\Nats\Connection();
$connection->sendMessage(new PriorityPublish());
chronos_request_end(200, "");
PHP);
    $headers = lastJson($out);
    assertTrue(is_array($headers), 'subclass payload->headers readable in the method body');
    assertTrue(
        str_starts_with($headers['traceparent'] ?? '', '00-0af7651916cd43dd8448eb211c80319c-'),
        'a Publish subclass is still a Publish: injection reached $message->payload->headers',
    );
});

test('NATS Publish under chronos_suppress_native("messaging"): the seam demotes the branch — nothing injected', function (): void {
    [$out] = runInstrumented(NATS_CLASSES."\n".OPEN_REQUEST.<<<'PHP'

chronos_suppress_native("messaging");
$connection = new \Basis\Nats\Connection();
$connection->sendMessage(new \Basis\Nats\Message\Publish());
chronos_request_end(200, "");
PHP);
    assertSame([], lastJson($out), 'suppressed publish wrote no header at all');
});

const AMQPLIB_CLASSES = <<<'PHP'
namespace PhpAmqpLib\Message;
class AMQPMessage {
    public string $body = "amqp-body";
    protected array $properties;
    public function __construct(array $properties = []) { $this->properties = $properties; }
    public function props(): array { return $this->properties; }
}

namespace PhpAmqpLib\Wire;
class AMQPTable {
    protected array $data;
    public function __construct(array $data) { $this->data = $data; }
    public function raw(): array { return $this->data; }
}

namespace PhpAmqpLib\Channel;
class AMQPChannel {
    public function basic_publish(object $msg, string $exchange = "", string $routing_key = ""): void {
        $headers = $msg->props()["application_headers"] ?? null;
        echo json_encode($headers instanceof \PhpAmqpLib\Wire\AMQPTable ? $headers->raw() : $headers), "\n";
    }
}
PHP;

test('amqplib absent application_headers: a fresh plain array of [S, value] tuples is written', function (): void {
    [$out] = runInstrumented(AMQPLIB_CLASSES."\n".OPEN_REQUEST.<<<'PHP'

$channel = new \PhpAmqpLib\Channel\AMQPChannel();
$channel->basic_publish(new \PhpAmqpLib\Message\AMQPMessage(), "organizations", "order.created");
chronos_request_end(200, "");
PHP);
    $headers = lastJson($out);
    assertTrue(is_array($headers), 'application_headers written where none existed');
    assertSame('S', $headers['traceparent'][0] ?? null, 'legacy symbol-char tuple tag');
    assertTrue(
        str_starts_with($headers['traceparent'][1] ?? '', '00-0af7651916cd43dd8448eb211c80319c-'),
        'traceparent tuple value continues the inbound trace',
    );
    assertSame('S', $headers['x-chronos-enqueued-at'][0] ?? null, 'stamp shares the tuple shape');
});

test('amqplib AMQPTable object: add-if-absent [14, value] tuples land in its own $data, caller keys kept', function (): void {
    [$out] = runInstrumented(AMQPLIB_CLASSES."\n".OPEN_REQUEST.<<<'PHP'

$table = new \PhpAmqpLib\Wire\AMQPTable(["traceparent" => [14, "00-caller-owns-this-01"]]);
$channel = new \PhpAmqpLib\Channel\AMQPChannel();
$channel->basic_publish(new \PhpAmqpLib\Message\AMQPMessage(["application_headers" => $table]), "ex", "rk");
chronos_request_end(200, "");
PHP);
    $data = lastJson($out);
    assertSame([14, '00-caller-owns-this-01'], $data['traceparent'] ?? null, 'caller tuple untouched');
    assertSame(14, $data['x-chronos-enqueued-at'][0] ?? null, 'added entries use the AMQPTable int-tag shape');
    assertTrue(isset($data['x-chronos-enqueued-at'][1]), 'stamp value present');
});

fwrite(STDOUT, "\n{$tests} cases, {$failures} failed\n");
exit($failures > 0 ? 1 : 0);
