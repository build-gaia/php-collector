<?php

declare(strict_types=1);

/**
 * Standalone verification for Framework/Bunny (BunnyTelemetry) and the messaging
 * vocabulary it leans on (MessagingDestination::forAmqp, MessagingBody, MessagingSpan's
 * span-name fallback).
 *
 * No PHPUnit and no vendor directory (see verify.php's own header for why), and no real
 * bunny/bunny install to run against either: this package has zero runtime dependencies, so
 * the tiniest possible stand-ins for Bunny\Channel/Message/Client are declared below — the
 * same public property names, the same `publish()` parameter order, the same untyped
 * `getHeader()` — just enough of Bunny's real shape for BunnyTelemetry to run against
 * unmodified. They are declared BEFORE the autoloader ever pulls in BunnyTelemetry.php, so the
 * `class_exists()` guard that file is wrapped in sees real classes and the guarded body
 * actually declares — proving the INSTALLED path, not just the absent no-op.
 *
 * The native .so is not loaded here (`extension_loaded('chronos')` is false in this process
 * regardless), so NativeExtension is permanently in its fail-open state: requestStart/
 * requestEnd/setRequestAttributes/active() are no-ops and childTraceparent() returns null. That
 * is exactly the contract under test for the pass-through paths — a message must reach its
 * handler and a handler's exception must reach the caller whether or not telemetry is doing
 * anything. Where a test needs the OTHER state (capture on, a span actually recording), the
 * memoised statics are set through reflection rather than faked at a higher level, so the code
 * under test is the real code: messenger-case.php does the same for its unobservable privates.
 *
 * exit(1) with a message on the first failure, matching this suite's style.
 */

// ---------------------------------------------------------------------------
// Native seam stub. SpanManager::complete() hands a finished span to
// NativeExtension::recordSpan() whenever the extension reports as loaded, which is
// the state the capture-on cases force — so the span crosses the FFI instead of
// landing in the userland buffer. Stubbing the global function is how this suite
// already observes that seam (see integration-wiring-case.php).
// ---------------------------------------------------------------------------

namespace {
    $GLOBALS['chronos_recorded_spans'] = [];

    // Guarded: a real chronos.so already defines this, and redeclaring it is a fatal.
    // Where the extension IS loaded the native batch owns finished spans and there is
    // nothing for this suite to read, so the span-observing cases report as skipped
    // rather than quietly asserting nothing — the same honesty native-extension-case.php
    // applies to its own live sections.
    if (!function_exists('chronos_record_span')) {
        function chronos_record_span(
            string $traceId,
            string $spanId,
            string $parentSpanId,
            string $name,
            string $startedAt,
            string $endedAt,
            array $attributes,
            string $status,
        ): void {
            $GLOBALS['chronos_recorded_spans'][] = [
                'name' => $name,
                'attributes' => $attributes,
                // The span's OWN identity, which the causality cases below compare
                // against the traceparent that went out on the wire — the whole
                // point of a reservation is that those two are the same id.
                'traceId' => $traceId,
                'spanId' => $spanId,
                'parentSpanId' => $parentSpanId,
                'status' => $status,
            ];
        }
    }

    // ---------------------------------------------------------------------
    // The rest of the native seam the new behaviour needs. Each is guarded, for
    // the reason above: a real chronos.so already defines them and redeclaring
    // one is a fatal. Every stub reads its answer out of $GLOBALS so a case can
    // set the state it needs and put it back.
    // ---------------------------------------------------------------------

    $GLOBALS['chronos_traceparent'] = '';
    $GLOBALS['chronos_stored_bodies'] = [];
    $GLOBALS['chronos_store_answer'] = true;
    $GLOBALS['chronos_request_ends'] = [];
    $GLOBALS['chronos_request_open'] = false;

    if (!function_exists('chronos_traceparent')) {
        function chronos_traceparent(): string
        {
            return (string) ($GLOBALS['chronos_traceparent'] ?? '');
        }
    }

    if (!function_exists('chronos_store_span_body')) {
        function chronos_store_span_body(
            string $traceId,
            string $spanId,
            string $side,
            string $contentType,
            string $body,
            string $encoding,
        ): bool {
            $GLOBALS['chronos_stored_bodies'][] = [
                'traceId' => $traceId,
                'spanId' => $spanId,
                'side' => $side,
                'contentType' => $contentType,
                'body' => $body,
                'encoding' => $encoding,
            ];

            return (bool) ($GLOBALS['chronos_store_answer'] ?? true);
        }
    }

    if (!function_exists('chronos_setting')) {
        function chronos_setting(string $name): string
        {
            return $name === 'CHRONOS_PHP_ENABLED' ? '1' : '';
        }
    }

    if (!function_exists('chronos_request_start')) {
        function chronos_request_start(...$arguments): void
        {
            $GLOBALS['chronos_request_open'] = true;
        }
    }

    if (!function_exists('chronos_request_active')) {
        function chronos_request_active(): bool
        {
            return (bool) ($GLOBALS['chronos_request_open'] ?? false);
        }
    }

    if (!function_exists('chronos_request_end')) {
        function chronos_request_end(
            int $status,
            string $route = '',
            ?string $errorType = null,
            ?string $errorMessage = null,
            ?string $errorStack = null,
            ?string $errorCode = null,
            $errorExitCode = null,
            ?bool $handled = null,
        ): void {
            $GLOBALS['chronos_request_open'] = false;
            $GLOBALS['chronos_request_ends'][] = [
                'route' => $route,
                'errorType' => $errorType,
                'errorMessage' => $errorMessage,
                'handled' => $handled,
            ];
        }
    }
}

namespace Bunny {
    /** Records the six arguments Bunny's own publish() takes, and answers like confirm mode. */
    class Channel
    {
        /** @var list<array<string, mixed>> */
        public array $published = [];

        public function publish(
            $body,
            array $headers = [],
            $exchange = '',
            $routingKey = '',
            $mandatory = false,
            $immediate = false,
        ) {
            $this->published[] = [
                'body' => $body,
                'headers' => $headers,
                'exchange' => $exchange,
                'routingKey' => $routingKey,
                'mandatory' => $mandatory,
                'immediate' => $immediate,
            ];

            return 1;
        }
    }

    class Client
    {
    }

    /** Bunny's Message verbatim in shape: public properties, untyped getHeader(). */
    class Message
    {
        public function __construct(
            public $consumerTag = 'ctag',
            public $deliveryTag = 1,
            public $redelivered = false,
            public $exchange = '',
            public $routingKey = '',
            public array $headers = [],
            public $content = '',
        ) {
        }

        public function getHeader($name, $default = null)
        {
            return $this->headers[$name] ?? $default;
        }

        public function hasHeader($name)
        {
            return isset($this->headers[$name]);
        }
    }
}

namespace Chronos\Collector\Tests {

use Bunny\Channel;
use Bunny\Client;
use Bunny\Message;
use Chronos\Collector\Dto\SpanReservation;
use Chronos\Collector\Framework\Bunny\BunnyTelemetry;
use Chronos\Collector\Service\MessagingBody;
use Chronos\Collector\Service\MessagingDestination;
use Chronos\Collector\Service\MessagingFailure;
use Chronos\Collector\Service\MessagingSpan;
use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;

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

final class Runner
{
    private int $failures = 0;

    public function test(string $name, callable $test): void
    {
        try {
            $test();
            fwrite(STDOUT, "PASS {$name}\n");
        } catch (\Throwable $error) {
            ++$this->failures;
            fwrite(STDERR, "FAIL {$name}: {$error->getMessage()}\n");
        }
    }

    /** Report a case as not run, with the reason — never as a pass. */
    public function skip(string $name, string $why): void
    {
        fwrite(STDOUT, "SKIP {$name}: {$why}\n");
    }

    public function assertTrue(bool $condition, string $message): void
    {
        if (!$condition) {
            throw new \RuntimeException($message);
        }
    }

    public function assertSame(mixed $expected, mixed $actual, string $context = ''): void
    {
        if ($expected !== $actual) {
            $expectedText = is_scalar($expected) || $expected === null ? var_export($expected, true) : get_debug_type($expected);
            $actualText = is_scalar($actual) || $actual === null ? var_export($actual, true) : get_debug_type($actual);
            throw new \RuntimeException("{$context}expected {$expectedText}, got {$actualText}");
        }
    }

    public function exit(): never
    {
        if ($this->failures > 0) {
            fwrite(STDERR, "{$this->failures} failure(s)\n");
            exit(1);
        }
        fwrite(STDOUT, "All bunny tests passed\n");
        exit(0);
    }
}

/**
 * Force NativeExtension's memoised payload-capture answers.
 *
 * Reflection rather than environment, because `messagingCapturing()` gates on
 * `loaded()` first and no amount of env will make `extension_loaded('chronos')` true in this
 * process. Setting the memos directly is what lets the ENCODING — the part worth pinning down
 * — be tested against the real MessagingBody, with the real gate in front of it, in a process
 * with no extension and no broker. Pass null to put them back the way they were.
 */
function captureBodies(?bool $on, int $ceiling = 65536): void
{
    foreach ([['loaded', $on], ['messagingCapture', $on], ['messagingCeiling', $on === null ? null : $ceiling]] as [$name, $value]) {
        (new \ReflectionProperty(NativeExtension::class, $name))->setValue(null, $value);
    }
}

/**
 * Run $fn with a real request-scoped span stack open, and return the spans it finished as
 * `['name' => ..., 'attributes' => ...]`.
 *
 * Two collection points, because SpanManager::complete() legitimately has two: it buffers into
 * its own static when no extension is loaded, and bridges across the FFI when one is. The
 * capture-on cases force `loaded` on (there is no other way to reach the payload gate without
 * a built .so), so those spans arrive through the stubbed `chronos_record_span` while every
 * other case arrives through the buffer. Reading both means a test asserts on the span that
 * would really have shipped, whichever path it took.
 *
 * A root has to be opened first: SpanManager::spawn() answers Span::null() with no parent, so a
 * publish span outside a request is void by design — which is itself the documented risk about
 * publishes from CLI commands.
 *
 * @return list<array{name: string, attributes: array<string, string>}>
 */
function recordedSpans(callable $fn): array
{
    $GLOBALS['chronos_recorded_spans'] = [];
    SpanManager::begin(Span::open('trace-id', 'root-span', '', 'root'));
    try {
        $fn();
    } finally {
        $buffered = SpanManager::end();
    }
    $spans = $GLOBALS['chronos_recorded_spans'];
    foreach ($buffered as $record) {
        $spans[] = ['name' => $record->name, 'attributes' => $record->attributes];
    }

    return $spans;
}

/**
 * Whether finished spans are observable in this process.
 *
 * SpanManager::complete() hands a span to the native batch whenever the extension reports as
 * loaded, and the capture-on cases have to force that flag to reach the payload gate. With a
 * REAL .so loaded the span is gone into the extension and this suite cannot read it back, so
 * those cases are skipped rather than asserted vacuously.
 */
function spansObservable(): bool
{
    return !\extension_loaded('chronos') && !\extension_loaded('chronos-ext');
}

$runner = new Runner();

$runner->test('the guarded class really declared, so the installed path is what is under test', function () use ($runner): void {
    // If this fails every other case below is vacuously green: the class_exists() guard
    // would have skipped the declaration and nothing would exist to call.
    $runner->assertTrue(class_exists(BunnyTelemetry::class), 'BunnyTelemetry must declare when Bunny\\Channel and Bunny\\Message exist');
});

$runner->test('with bunny absent the file loads, declares nothing, and cannot fatal', function () use ($runner): void {
    // The state that matters most in production: an application that does not depend on
    // bunny/bunny at all. A CHILD process is the only honest way to assert it, because this
    // one has already declared the Bunny stand-ins. Requiring the file must be silent, and
    // the class must simply not exist — never a "class not found" at load.
    $file = \escapeshellarg(\dirname(__DIR__).'/src/Framework/Bunny/BunnyTelemetry.php');
    $script = "require {$file}; echo class_exists('Chronos\\\\Collector\\\\Framework\\\\Bunny\\\\BunnyTelemetry') ? 'declared' : 'absent';";
    $output = [];
    $status = 0;
    \exec(\PHP_BINARY.' -r '.\escapeshellarg($script).' 2>&1', $output, $status);
    $runner->assertSame(0, $status, 'loading the bridge without bunny must exit cleanly, got: '.implode("\n", $output));
    $runner->assertSame('absent', trim(implode('', $output)), 'the guard must skip the declaration entirely: ');
});

$runner->test('publish forwards all six Bunny arguments unchanged and returns the channel\'s answer', function () use ($runner): void {
    $channel = new Channel();

    $result = BunnyTelemetry::publish(
        $channel,
        'the-body',
        ['x-custom' => 'kept'],
        'organizations',
        'order.created',
        true,
        false,
        vhost: 'oms',
        messageName: 'Qls\\Protocol\\OrderCreated',
        contentType: 'application/x-protobuf',
    );

    $runner->assertSame(1, $result, 'the channel\'s own return value must pass back untouched: ');
    $runner->assertSame(1, count($channel->published), 'exactly one publish must reach the channel: ');
    $sent = $channel->published[0];
    $runner->assertSame('the-body', $sent['body']);
    $runner->assertSame('organizations', $sent['exchange']);
    $runner->assertSame('order.created', $sent['routingKey']);
    $runner->assertSame(true, $sent['mandatory']);
    $runner->assertSame(false, $sent['immediate']);
    $runner->assertSame('kept', $sent['headers']['x-custom'] ?? null, 'the caller\'s own headers must survive: ');
});

$runner->test('publish stamps the enqueued-at instant even with no open request', function () use ($runner): void {
    // No .so, so childTraceparent() is null and there is no trace to continue — but the
    // message has still waited, and the consume side can only measure that if the stamp is
    // there. QueueTelemetry::payloadContext() makes the same argument for a CLI dispatch.
    $channel = new Channel();
    BunnyTelemetry::publish($channel, 'x', [], '', 'asn-items');
    $stamp = $channel->published[0]['headers'][BunnyTelemetry::ENQUEUED_AT_HEADER] ?? null;
    $runner->assertTrue(is_string($stamp) && is_numeric($stamp), 'x-chronos-enqueued-at must be a numeric wall-clock stamp');
    $runner->assertTrue((float) $stamp > 1_600_000_000.0, 'the stamp must be epoch seconds, not a monotonic reading');
});

$runner->test('a caller-supplied traceparent is never overwritten by instrumentation', function () use ($runner): void {
    // The injection is `$headers + contextHeaders()`, a union: the caller wins. Rewriting an
    // application's own propagation would break its trace while looking like it worked.
    $channel = new Channel();
    BunnyTelemetry::publish($channel, 'x', ['traceparent' => '00-caller-owns-this-01'], '', 'q');
    $runner->assertSame('00-caller-owns-this-01', $channel->published[0]['headers']['traceparent'] ?? null);
});

$runner->test('publish never lets a recording failure surface as a send failure', function () use ($runner): void {
    // MessagingSpan degrades to a void span with no extension loaded; that must not reach
    // the caller in any form, and the channel must still have been called.
    $channel = new Channel();
    $runner->assertSame(1, BunnyTelemetry::publish($channel, 'x', [], 'ex', 'rk'));
    $runner->assertSame(1, count($channel->published));
});

$runner->test('forAmqp leaves the queue ABSENT for a topic publish and names via + route', function () use ($runner): void {
    // A topic publish has no queue at all: zero, one or six queues may be bound to that
    // routing key, and the publisher cannot know which. Filling NAME here would attach the
    // trace to a stream confidently and wrongly.
    $attributes = MessagingDestination::forAmqp('oms', 'organizations', 'order.created');
    $runner->assertSame(false, isset($attributes[MessagingDestination::NAME]), 'a topic publish must carry no destination name: ');
    $runner->assertSame('oms', $attributes[MessagingDestination::NAMESPACE_KEY] ?? null);
    $runner->assertSame('organizations', $attributes[MessagingDestination::VIA] ?? null);
    $runner->assertSame('order.created', $attributes[MessagingDestination::ROUTE] ?? null);
});

$runner->test('forAmqp makes the routing key the queue on the default exchange, and omits via', function () use ($runner): void {
    // The nameless exchange is the one case where a routing key IS a queue name — that is
    // what it does. `via` stays absent rather than becoming `amq.default`, because the
    // nameless exchange has no name to record.
    $attributes = MessagingDestination::forAmqp('oms', '', 'asn-items');
    $runner->assertSame('asn-items', $attributes[MessagingDestination::NAME] ?? null);
    $runner->assertSame('asn-items', $attributes[MessagingDestination::ROUTE] ?? null, 'route is kept too — the duplication is how the default exchange routes: ');
    $runner->assertSame(false, isset($attributes[MessagingDestination::VIA]), 'the default exchange must not be named: ');
});

$runner->test('forAmqp keeps an explicitly known queue whatever the exchange was', function () use ($runner): void {
    $attributes = MessagingDestination::forAmqp('oms', 'organizations', 'order.created', 'oms-orders');
    $runner->assertSame('oms-orders', $attributes[MessagingDestination::NAME] ?? null);
    $runner->assertSame('organizations', $attributes[MessagingDestination::VIA] ?? null);
});

$runner->test('forAmqp omits the namespace rather than guessing the default vhost', function () use ($runner): void {
    $attributes = MessagingDestination::forAmqp('', 'organizations', 'order.created');
    $runner->assertSame(false, isset($attributes[MessagingDestination::NAMESPACE_KEY]), 'an unknown vhost must be absent, never "/": ');
});

$runner->test('a queueless publish is named after the exchange, not after the broker', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('a queueless publish is named after the exchange', 'a real chronos extension owns the span batch here');

        return;
    }
    // "PUBLISH rabbitmq" would collapse every publish in the estate into one row in the
    // trace list, which destroys exactly the discrimination a span name exists to provide.
    $spans = recordedSpans(static function (): void {
        MessagingSpan::published('rabbitmq', '', 'X', [MessagingDestination::VIA => 'organizations']);
    });
    $runner->assertSame(1, count($spans), 'one producer span must have been recorded: ');
    $runner->assertSame('PUBLISH organizations', $spans[0]['name']);
});

$runner->test('the span-name fallback walks destination -> via -> route -> system', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('the span-name fallback walks destination -> via -> route -> system', 'a real chronos extension owns the span batch here');

        return;
    }
    $names = [];
    foreach ([
        ['oms-orders', [MessagingDestination::VIA => 'organizations', MessagingDestination::ROUTE => 'order.created']],
        ['', [MessagingDestination::VIA => 'organizations', MessagingDestination::ROUTE => 'order.created']],
        ['', [MessagingDestination::ROUTE => 'asn-items']],
        ['', []],
    ] as [$destination, $extra]) {
        $spans = recordedSpans(static function () use ($destination, $extra): void {
            MessagingSpan::published('rabbitmq', $destination, '', $extra);
        });
        $names[] = $spans[0]['name'];
    }
    $runner->assertSame(
        ['PUBLISH oms-orders', 'PUBLISH organizations', 'PUBLISH asn-items', 'PUBLISH rabbitmq'],
        $names,
        'the fallback must stop at the first bounded identity it has, with $system as the floor: ',
    );
});

$runner->test('with capture OFF a protobuf body is sized but never carried', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('with capture OFF a protobuf body is sized but never carried', 'a real chronos extension owns the span batch here');

        return;
    }
    captureBodies(false);
    try {
        $spans = recordedSpans(static function (): void {
            $channel = new Channel();
            BunnyTelemetry::publish(
                $channel,
                "\x08\x96\x01\xff\xfe",
                [],
                'organizations',
                'order.created',
                vhost: 'oms',
                contentType: 'application/x-protobuf',
            );
        });
        $attributes = $spans[0]['attributes'];
        $runner->assertSame(false, isset($attributes[MessagingBody::BODY]), 'no payload may be emitted with capture off: ');
        $runner->assertSame('5', $attributes['messaging.message.body.size'] ?? null, 'the size survives the gate: ');
        $runner->assertSame('protobuf', $attributes['messaging.protocol'] ?? null);
    } finally {
        captureBodies(null);
    }
});

$runner->test('with capture ON a protobuf body goes out base64-encoded and marked as such', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip(
            'with capture ON a protobuf body goes out base64-encoded',
            'a real chronos extension owns the span batch here; the encoding itself is covered by the MessagingBody cases below',
        );

        return;
    }
    captureBodies(true);
    try {
        $spans = recordedSpans(static function (): void {
            $channel = new Channel();
            BunnyTelemetry::publish(
                $channel,
                "\x08\x96\x01\xff\xfe",
                [],
                'organizations',
                'order.created',
                vhost: 'oms',
                contentType: 'application/x-protobuf',
            );
        });
        $attributes = $spans[0]['attributes'];
        $runner->assertSame(base64_encode("\x08\x96\x01\xff\xfe"), $attributes[MessagingBody::BODY] ?? null);
        $runner->assertSame('base64', $attributes[MessagingBody::ENCODING] ?? null);
        $runner->assertSame(false, isset($attributes[MessagingBody::TRUNCATED]), 'a body inside the cap is not truncated: ');
        // Not valid UTF-8, which is the whole reason it cannot ride raw: it has to survive
        // ext-php-rs into a Rust String and then serde_json.
        $runner->assertSame(false, preg_match('//u', "\x08\x96\x01\xff\xfe") === 1, 'the fixture must really be non-UTF-8: ');
    } finally {
        captureBodies(null);
    }
});

$runner->test('a JSON body rides as text with no encoding key, and is cut on a character boundary', function () use ($runner): void {
    captureBodies(true, 64);
    try {
        // A multibyte character straddling the cap is the case a byte-wise cut breaks: the
        // attribute would stop being valid UTF-8 and would be rejected or mangled downstream.
        $body = '{"city":"'.str_repeat('é', 40).'"}';
        $encoded = MessagingBody::encode($body, 16384);
        $value = $encoded[MessagingBody::BODY] ?? '';
        $runner->assertTrue($value !== '', 'a text body must be captured');
        $runner->assertSame(1, preg_match('//u', $value), 'the truncated text must still be valid UTF-8: ');
        $runner->assertTrue(strlen($value) <= 64, 'the cut must respect the byte budget, got '.strlen($value));
        $runner->assertSame('true', $encoded[MessagingBody::TRUNCATED] ?? null, 'a cut body must say so: ');
        $runner->assertSame(false, isset($encoded[MessagingBody::ENCODING]), 'text is the default and needs no encoding key: ');
    } finally {
        captureBodies(null);
    }
});

$runner->test('a base64 body is cut so the ENCODED value fits the caller\'s ceiling', function () use ($runner): void {
    captureBodies(true, 1024);
    try {
        // Encoding first and cutting after would overshoot by 1.33x — so the real cut would
        // happen later, in native code that cannot set .truncated — and could sever a
        // 4-character base64 group into something that no longer decodes.
        $raw = str_repeat("\x00\xff", 500);
        $encoded = MessagingBody::encode($raw, 40);
        $value = $encoded[MessagingBody::BODY] ?? '';
        $runner->assertTrue(strlen($value) <= 40, 'the encoded value must fit the ceiling, got '.strlen($value));
        $runner->assertSame('true', $encoded[MessagingBody::TRUNCATED] ?? null);
        $runner->assertTrue(base64_decode($value, true) !== false, 'the emitted base64 must still decode');
        $runner->assertTrue(str_starts_with($raw, (string) base64_decode($value, true)), 'it must decode to a genuine prefix of the payload');
    } finally {
        captureBodies(null);
    }
});

$runner->test('capture off means the body is never even looked at', function () use ($runner): void {
    captureBodies(false);
    try {
        $runner->assertSame([], MessagingBody::encode('anything at all', 16384));
    } finally {
        captureBodies(null);
    }
});

$runner->test('an absent extension reports capture off, whatever the setting says', function () use ($runner): void {
    // The fail-safe direction for a payload: an older .so with no chronos_setting(), or no
    // .so at all, must mean "do not send it" rather than "nobody said otherwise".
    $runner->assertSame(false, NativeExtension::messagingCapturing());
});

$runner->test('the wrapped consumer reaches the handler and returns its result', function () use ($runner): void {
    $seen = null;
    $callback = BunnyTelemetry::consumer(static function (Message $message) use (&$seen): string {
        $seen = $message;

        return 'handled';
    }, 'oms-orders', 'oms');

    $message = new Message(routingKey: 'order.created', headers: ['traceparent' => '00-a-b-01'], content: '{}');
    $runner->assertSame('handled', $callback($message));
    $runner->assertTrue($seen === $message, 'the handler must receive the very message Bunny delivered');
});

$runner->test('a one-parameter handler still runs when Bunny calls the callback with three arguments', function () use ($runner): void {
    // Channel.php:743 invokes `$callback($message, $this, $this->client)`. The wrapper takes
    // ...$rest and forwards everything; PHP discards extra arguments to a userland function,
    // so an application closure declaring one parameter keeps working unchanged.
    $ran = false;
    $callback = BunnyTelemetry::consumer(static function (Message $message) use (&$ran): void {
        $ran = true;
    }, 'oms-orders');

    $callback(new Message(), new Channel(), new Client());
    $runner->assertSame(true, $ran, 'the handler must have run: ');
});

$runner->test('the wrapped consumer rethrows the handler\'s throwable untouched', function () use ($runner): void {
    $boom = new \RuntimeException('boom');
    $callback = BunnyTelemetry::consumer(static function (Message $message) use ($boom): void {
        throw $boom;
    }, 'oms-orders');

    $caught = null;
    try {
        $callback(new Message(), new Channel(), new Client());
    } catch (\Throwable $e) {
        $caught = $e;
    }
    $runner->assertTrue($caught === $boom, 'the exact exception instance must reach the caller, not a copy or a rewrap');
});

$runner->test('a non-string AMQP header cannot break the consume path', function () use ($runner): void {
    // ContentHeaderFrame::toArray() merges the AMQP properties and the nested field table
    // into $headers, so a value can legitimately be an int, a DateTime or an array.
    $ran = false;
    $callback = BunnyTelemetry::consumer(static function (Message $message) use (&$ran): void {
        $ran = true;
    }, 'oms-orders');

    $callback(new Message(headers: [
        'traceparent' => ['not', 'a', 'string'],
        'delivery-mode' => 2,
        'timestamp' => new \DateTimeImmutable(),
        'message-id' => 'msg-1',
    ]));
    $runner->assertSame(true, $ran, 'the handler must still run: ');
});

$runner->test('consumeAttributes describes the delivery in the shared vocabulary', function () use ($runner): void {
    $message = new Message(
        redelivered: true,
        exchange: 'organizations',
        routingKey: 'order.created',
        headers: [
            'message-id' => 'msg-1',
            'correlation-id' => 'conv-9',
            'content-type' => 'application/x-protobuf',
            BunnyTelemetry::ENQUEUED_AT_HEADER => sprintf('%.6F', microtime(true) - 1.5),
        ],
        content: 'abc',
    );

    $method = new \ReflectionMethod(BunnyTelemetry::class, 'consumeAttributes');
    /** @var array<string, string> $attributes */
    $attributes = $method->invoke(null, $message, 'oms-orders', 'oms-orders', 'oms', '', microtime(true));

    $runner->assertSame('consumer', $attributes['span.kind'] ?? null);
    $runner->assertSame('rabbitmq', $attributes['messaging.system'] ?? null);
    $runner->assertSame('process', $attributes['messaging.operation'] ?? null);
    // The known queue wins as the leaf, while via and route come off the delivery — so both
    // halves of one stream describe the same place in the same four keys.
    $runner->assertSame('oms-orders', $attributes[MessagingDestination::NAME] ?? null);
    $runner->assertSame('oms', $attributes[MessagingDestination::NAMESPACE_KEY] ?? null);
    $runner->assertSame('organizations', $attributes[MessagingDestination::VIA] ?? null);
    $runner->assertSame('order.created', $attributes[MessagingDestination::ROUTE] ?? null);
    $runner->assertSame('true', $attributes['messaging.message.redelivered'] ?? null);
    $runner->assertSame('msg-1', $attributes['messaging.message.id'] ?? null);
    $runner->assertSame('conv-9', $attributes['messaging.message.conversation_id'] ?? null);
    $runner->assertSame('protobuf', $attributes['messaging.protocol'] ?? null);
    $runner->assertSame('3', $attributes['messaging.message.body.size'] ?? null);
    $runner->assertTrue(
        (int) ($attributes['messaging.message.queue_time_ms'] ?? 0) >= 1400,
        'the queue wait must be read off the publish stamp, got '.var_export($attributes['messaging.message.queue_time_ms'] ?? null, true),
    );
    // AMQP has no consumer groups, and the delivery tag is per-connection and unbounded.
    $runner->assertSame(false, isset($attributes['messaging.consumer.group.name']), 'no invented consumer group: ');
    $runner->assertSame(false, isset($attributes['messaging.message.delivery_tag']), 'no unbounded delivery tag: ');
    // QLS sends no AMQP `type`, so the message name is absent rather than back-filled from
    // the queue — a queue is a place, not a message type.
    $runner->assertSame(false, isset($attributes['messaging.message.name']), 'no message name guessed from the queue: ');
});

$runner->test('routeName prefers the subscribed queue and never falls back to a tag', function () use ($runner): void {
    $method = new \ReflectionMethod(BunnyTelemetry::class, 'routeName');
    $runner->assertSame('oms-orders', $method->invoke(null, new Message(routingKey: 'order.created'), 'oms-orders'));
    $runner->assertSame('order.created', $method->invoke(null, new Message(routingKey: 'order.created'), ''));
    $runner->assertSame('organizations', $method->invoke(null, new Message(exchange: 'organizations'), ''));
    $runner->assertSame('amqp', $method->invoke(null, new Message(), ''));
});

// ---------------------------------------------------------------------------
// CAUSALITY: the traceparent on the wire must name a span that is really recorded.
// ---------------------------------------------------------------------------

/**
 * Force NativeExtension's memoised `loaded` flag, and the trace context the stubbed
 * `chronos_traceparent()` hands back.
 *
 * Reflection for the same reason captureBodies() uses it: no amount of environment makes
 * `extension_loaded('chronos')` true in this process, and `loaded()` is the first gate on every
 * seam these cases exercise. Pass null to put both back.
 */
function nativeRequest(?string $traceparent): void
{
    (new \ReflectionProperty(NativeExtension::class, 'loaded'))->setValue(null, $traceparent === null ? null : true);
    (new \ReflectionProperty(NativeExtension::class, 'enabled'))->setValue(null, $traceparent === null ? null : true);
    $GLOBALS['chronos_traceparent'] = $traceparent ?? '';
    $GLOBALS['chronos_request_open'] = false;
    // Only on SETUP. Clearing these on teardown too would wipe the very
    // observations the case is about to assert on.
    if ($traceparent !== null) {
        $GLOBALS['chronos_stored_bodies'] = [];
        $GLOBALS['chronos_request_ends'] = [];
    }
    SpanManager::reset();
    MessagingFailure::reset();
}

/** A well-formed 32-hex trace id and 16-hex span id, so a real W3C header can be asserted on. */
const TRACE_ID = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
const ROOT_SPAN_ID = '0102030405060708';
const ROOT_TRACEPARENT = '00-'.TRACE_ID.'-'.ROOT_SPAN_ID.'-01';

/**
 * Run a publish with a real request open, and return both halves of the join:
 * the AMQP headers that went to the broker, and the spans that were recorded.
 *
 * @return array{headers: array<string, mixed>, spans: list<array<string, mixed>>}
 */
function publishWithRequest(callable $publish, string $traceparent = ROOT_TRACEPARENT): array
{
    nativeRequest($traceparent);
    $GLOBALS['chronos_recorded_spans'] = [];
    $channel = new Channel();
    SpanManager::begin(Span::open(TRACE_ID, ROOT_SPAN_ID, '', 'request'));
    try {
        $publish($channel);
    } finally {
        SpanManager::end();
    }
    $result = [
        'headers' => $channel->published[0]['headers'] ?? [],
        'spans' => $GLOBALS['chronos_recorded_spans'],
    ];
    nativeRequest(null);

    return $result;
}

$runner->test('the traceparent on the wire names the publish span that is really recorded', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('the traceparent on the wire names the publish span', 'a real chronos extension owns the span batch here');

        return;
    }
    // THE BUG, in one assertion. Until the reservation existed the middle field of this header
    // was a fresh id from chronos_child_traceparent() that nothing was ever recorded under, so
    // the consumer's root span pointed at a span that did not exist: publish and consume shared
    // a trace id and had no parent/child edge at all.
    $result = publishWithRequest(static function (Channel $channel): void {
        BunnyTelemetry::publish($channel, 'x', [], 'organizations', 'order.created', vhost: 'oms');
    });

    $header = $result['headers'][BunnyTelemetry::TRACEPARENT_HEADER] ?? null;
    $runner->assertTrue(is_string($header), 'a publish inside a request must carry a traceparent');
    $onTheWire = SpanReservation::fromTraceparent((string) $header);
    $runner->assertTrue($onTheWire !== null, 'the header must be a valid W3C traceparent, got '.var_export($header, true));
    $runner->assertSame(1, count($result['spans']), 'exactly one producer span: ');
    $span = $result['spans'][0];
    $runner->assertSame('PUBLISH organizations', $span['name']);
    $runner->assertSame($onTheWire->spanId, $span['spanId'], 'the wire id must BE the recorded publish span id: ');
    $runner->assertSame(TRACE_ID, $span['traceId'], 'and it stays in the publisher trace: ');
    // The publish span parents onto the request root, so the consumer nests one level deeper
    // than the request rather than beside it.
    $runner->assertSame(ROOT_SPAN_ID, $span['parentSpanId'], 'the publish span hangs off the request root: ');
    $runner->assertSame('producer', $span['attributes']['span.kind'] ?? null);
});

$runner->test('the wire carries the publisher\'s own sampled flag, unmodified', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('the wire carries the publisher\'s own sampled flag', 'a real chronos extension owns the span batch here');

        return;
    }
    // Propagating 00 is what makes an unsampled trace get dropped WHOLE: native start_request
    // only re-rolls the sampling die when there is no inbound parent, so a consumer with a
    // parent honours 00 and emits nothing. Forcing 01 would produce a half-recorded trace.
    $result = publishWithRequest(
        static function (Channel $channel): void {
            BunnyTelemetry::publish($channel, 'x', [], '', 'asn-items');
        },
        '00-'.TRACE_ID.'-'.ROOT_SPAN_ID.'-00',
    );
    $header = (string) ($result['headers'][BunnyTelemetry::TRACEPARENT_HEADER] ?? '');
    $runner->assertTrue(str_ends_with($header, '-00'), 'an unsampled publisher must propagate 00, got '.$header);
});

$runner->test('with the span budget spent the REQUEST ROOT is propagated, never a fabricated id', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('with the span budget spent the request root is propagated', 'a real chronos extension owns the span batch here');

        return;
    }
    // Tier 2. reserve() refuses once MAX_SPANS is committed, and the fallback names the request
    // ROOT — a span the extension always emits for a sampled request. Less precise than the
    // publish call, still a real parent, which is the whole distinction from the old phantom.
    nativeRequest(ROOT_TRACEPARENT);
    $channel = new Channel();
    SpanManager::begin(Span::open(TRACE_ID, ROOT_SPAN_ID, '', 'request'));
    $held = [];
    for ($i = 0; $i < 80; ++$i) {
        $held[] = SpanManager::open('filler-'.$i);
    }
    $runner->assertTrue(SpanManager::reserve() === null, 'a spent budget must refuse to reserve');
    BunnyTelemetry::publish($channel, 'x', [], '', 'asn-items');
    SpanManager::end();
    $header = $channel->published[0]['headers'][BunnyTelemetry::TRACEPARENT_HEADER] ?? null;
    $runner->assertSame(ROOT_TRACEPARENT, $header, 'the degraded tier names the request root: ');
    nativeRequest(null);
});

$runner->test('with no request open at all, no traceparent is stamped', function () use ($runner): void {
    // Tier 3, and the rule that makes the other two trustworthy: a traceparent on the wire is a
    // PROMISE that the span it names will be recorded, and a publisher with no open request
    // cannot make that promise. The consumer roots its own trace, which is the honest outcome.
    $channel = new Channel();
    BunnyTelemetry::publish($channel, 'x', [], 'organizations', 'order.created');
    $runner->assertSame(
        false,
        isset($channel->published[0]['headers'][BunnyTelemetry::TRACEPARENT_HEADER]),
        'no open request must contribute no traceparent: ',
    );
    // The wait stamp is still there — it is measurable whether or not there is a trace.
    $runner->assertTrue(
        isset($channel->published[0]['headers'][BunnyTelemetry::ENQUEUED_AT_HEADER]),
        'the enqueued-at stamp does not depend on there being a trace',
    );
});

$runner->test('a caller\'s own traceparent still beats a reservation', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('a caller\'s own traceparent still beats a reservation', 'a real chronos extension owns the span batch here');

        return;
    }
    // The `$headers + contextHeaders()` union direction is unchanged by the reservation, so an
    // application doing its own propagation is still never silently rewritten.
    $result = publishWithRequest(static function (Channel $channel): void {
        BunnyTelemetry::publish($channel, 'x', ['traceparent' => '00-caller-owns-this-01'], '', 'q');
    });
    $runner->assertSame('00-caller-owns-this-01', $result['headers']['traceparent'] ?? null);
});

// ---------------------------------------------------------------------------
// PAYLOAD BLOBS
// ---------------------------------------------------------------------------

$runner->test('with capture ON the whole payload is stored against the publish span and claimed', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('the whole payload is stored against the publish span', 'a real chronos extension owns the span batch here');

        return;
    }
    captureBodies(true);
    try {
        // A payload well past the 16 KiB span-attribute ceiling: the preview is a stub, and the
        // blob is the only place the rest of it can live.
        $payload = str_repeat("\x08\x96\x01\xff\xfe", 8000);
        $result = publishWithRequest(static function (Channel $channel) use ($payload): void {
            BunnyTelemetry::publish($channel, $payload, [], 'organizations', 'order.created', vhost: 'oms', contentType: 'application/x-protobuf');
        });
        $span = $result['spans'][0];
        $stored = $GLOBALS['chronos_stored_bodies'];
        $runner->assertSame(1, count($stored), 'exactly one payload must be handed over: ');
        $runner->assertSame('message', $stored[0]['side'], 'side is one value, not publish/consume: ');
        $runner->assertSame($span['spanId'], $stored[0]['spanId'], 'the blob is keyed by the span that previews it: ');
        $runner->assertSame($span['traceId'], $stored[0]['traceId']);
        $runner->assertSame('application/x-protobuf', $stored[0]['contentType'], 'the DECLARED content type, never the transfer encoding: ');
        $runner->assertSame('base64', $stored[0]['encoding']);
        $runner->assertTrue(base64_decode($stored[0]['body'], true) !== false, 'the stored payload must decode');
        $runner->assertTrue(
            strlen((string) base64_decode($stored[0]['body'], true)) > strlen((string) base64_decode($span['attributes'][MessagingBody::BODY], true)),
            'the blob must hold more than the preview, or it buys nothing',
        );
        $runner->assertSame('true', $span['attributes'][MessagingBody::STORED] ?? null, 'a taken payload is claimed: ');
    } finally {
        captureBodies(null);
    }
});

$runner->test('a refused payload is never claimed on the span', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('a refused payload is never claimed', 'a real chronos extension owns the span batch here');

        return;
    }
    // The bool return is the whole reason the store has one. A `.stored` marker that resolves to
    // nothing is the single failure this store was built to prevent — the desktop would show a
    // "load the rest" action that 404s.
    captureBodies(true);
    $GLOBALS['chronos_store_answer'] = false;
    try {
        $result = publishWithRequest(static function (Channel $channel): void {
            BunnyTelemetry::publish($channel, str_repeat('{"a":1}', 4000), [], '', 'asn-items', contentType: 'application/json');
        });
        $runner->assertTrue(count($GLOBALS['chronos_stored_bodies']) >= 1, 'the payload was offered');
        $runner->assertSame(
            false,
            isset($result['spans'][0]['attributes'][MessagingBody::STORED]),
            'a refusal must not be claimed: ',
        );
    } finally {
        captureBodies(null);
        $GLOBALS['chronos_store_answer'] = true;
    }
});

$runner->test('with capture OFF not one byte is offered to the store', function () use ($runner): void {
    if (!spansObservable()) {
        $runner->skip('with capture OFF not one byte is offered to the store', 'a real chronos extension owns the span batch here');

        return;
    }
    captureBodies(false);
    try {
        $result = publishWithRequest(static function (Channel $channel): void {
            BunnyTelemetry::publish($channel, str_repeat('x', 100000), [], '', 'asn-items');
        });
        $runner->assertSame([], $GLOBALS['chronos_stored_bodies'], 'the default posture copies nothing: ');
        $runner->assertSame(false, isset($result['spans'][0]['attributes'][MessagingBody::STORED]));
        // The size still survives the gate, which is what lets the desktop explain that capture
        // is off rather than imply the message was empty.
        $runner->assertSame('100000', $result['spans'][0]['attributes']['messaging.message.body.size'] ?? null);
    } finally {
        captureBodies(null);
    }
});

$runner->test('whole() stores nothing when the allowance adds nothing over the preview', function () use ($runner): void {
    // http_capture's own rule, and for the same reason: a blob identical to the attribute beside
    // it costs a NATS message, a hypertable row and a round trip to say what the span already said.
    captureBodies(true, 8192);
    try {
        $runner->assertSame(['', ''], MessagingBody::whole(str_repeat('x', 50000), 16384));
        // Raise the allowance past the preview bound and the same payload IS worth storing.
        captureBodies(true, 65536);
        [$payload, $encoding] = MessagingBody::whole(str_repeat('x', 50000), 16384);
        $runner->assertSame(50000, strlen($payload), 'the whole payload fits the raised allowance: ');
        $runner->assertSame('', $encoding, 'text needs no transfer encoding: ');
    } finally {
        captureBodies(null);
    }
});

$runner->test('whole() cuts binary BEFORE base64 and text on a character boundary', function () use ($runner): void {
    captureBodies(true, 1024);
    try {
        // Same two rules encode() is held to, because they are the same implementation: a
        // preview and a stored copy that disagreed about one payload would be worse than either.
        [$binary, $encoding] = MessagingBody::whole(str_repeat("\x00\xff", 5000), 16);
        $runner->assertSame('base64', $encoding);
        $runner->assertTrue(strlen($binary) <= 1024, 'the ENCODED value must fit the allowance, got '.strlen($binary));
        $runner->assertTrue(base64_decode($binary, true) !== false, 'it must still decode');

        [$text] = MessagingBody::whole('{"city":"'.str_repeat('é', 2000).'"}', 16);
        $runner->assertSame(1, preg_match('//u', $text), 'a cut text payload must still be valid UTF-8: ');
        $runner->assertTrue(strlen($text) <= 1024, 'and must respect the byte budget, got '.strlen($text));
    } finally {
        captureBodies(null);
    }
});

$runner->test('the consume side stores its payload against the request ROOT span', function () use ($runner): void {
    captureBodies(true);
    $GLOBALS['chronos_stored_bodies'] = [];
    try {
        // Empty ids on purpose: the consume preview rides the request-attribute bag, whose 8 KiB
        // native cap is the tightest bound in this pipeline, and those values land on the ROOT.
        // Keying the blob anywhere else would split the preview from the payload it previews.
        $message = new Message(exchange: 'organizations', routingKey: 'order.created', headers: ['content-type' => 'application/x-protobuf'], content: str_repeat("\x08\x96\x01", 5000));
        $method = new \ReflectionMethod(BunnyTelemetry::class, 'consumeAttributes');
        $attributes = $method->invoke(null, $message, 'admin_oms_orders', 'admin_oms_orders', 'shared', '', microtime(true));

        $stored = $GLOBALS['chronos_stored_bodies'];
        $runner->assertSame(1, count($stored), 'the consume payload must be offered once: ');
        $runner->assertSame('', $stored[0]['spanId'], 'empty span id means "this request\'s root": ');
        $runner->assertSame('', $stored[0]['traceId']);
        $runner->assertSame('message', $stored[0]['side']);
        $runner->assertSame('base64', $stored[0]['encoding']);
        $runner->assertSame('true', $attributes[MessagingBody::STORED] ?? null, 'and the bag claims it: ');
    } finally {
        captureBodies(null);
        $GLOBALS['chronos_stored_bodies'] = [];
    }
});

// ---------------------------------------------------------------------------
// EXCEPTIONS: a swallowed delivery must not close as a success.
// ---------------------------------------------------------------------------

$runner->test('a delivery the application caught and acked closes as an ERROR', function () use ($runner): void {
    nativeRequest(ROOT_TRACEPARENT);
    try {
        // The application's own shape: catch every Throwable, report it, return normally so the
        // ack still runs. Before the failure slot the span closed with no error.type at all, so
        // a failed message was indistinguishable from a successful one.
        $boom = new \RuntimeException('protobuf decode failed');
        $callback = BunnyTelemetry::consumer(static function (Message $message) use ($boom): string {
            try {
                throw $boom;
            } catch (\Throwable $caught) {
                MessagingFailure::note($caught);
            }

            return 'acked';
        }, 'admin_oms_orders', 'shared');

        $runner->assertSame('acked', $callback(new Message()), 'the handler still returns normally, so the ack still runs: ');
        $ends = $GLOBALS['chronos_request_ends'];
        $runner->assertSame(1, count($ends), 'exactly one request end: ');
        $runner->assertSame('RuntimeException', $ends[0]['errorType'], 'the span must carry the error identity: ');
        $runner->assertSame('protobuf decode failed', $ends[0]['errorMessage']);
        // handled = true: the throwable never reached the transport. That is what separates
        // "failed and was swallowed" from "failed and killed the consumer" — only the second
        // stops a queue.
        $runner->assertSame(true, $ends[0]['handled']);
    } finally {
        nativeRequest(null);
    }
});

$runner->test('a successful delivery still closes clean, and the slot never leaks forward', function () use ($runner): void {
    nativeRequest(ROOT_TRACEPARENT);
    try {
        // The slot is a static inside a long-lived worker, so a failure nobody took would be
        // attributed to the NEXT message — a green delivery reported as red, which is worse than
        // the bug the slot fixes.
        MessagingFailure::note(new \RuntimeException('from a previous delivery'));
        $callback = BunnyTelemetry::consumer(static fn (Message $message): string => 'ok', 'admin_oms_orders');
        $runner->assertSame('ok', $callback(new Message()));
        $ends = $GLOBALS['chronos_request_ends'];
        $runner->assertSame(1, count($ends), 'exactly one request end: ');
        // array_key_exists, not `??`: the value under test IS null, which `??`
        // cannot tell apart from an absent key.
        $runner->assertTrue(array_key_exists('errorType', $ends[0]), 'the end was recorded');
        $runner->assertSame(null, $ends[0]['errorType'], 'a clean delivery reports no error: ');
    } finally {
        nativeRequest(null);
    }
});

$runner->test('a tier-2 stamp is never recovered as a reservation (it would duplicate the root span id)', function () use ($runner): void {
    // The one way the two tiers could collide. payloadContext() falls back to the REQUEST
    // ROOT's own traceparent when nothing could be reserved, and recovering THAT as a
    // reservation would record the producer span under the root's span id — two different spans
    // claiming one id in one trace, which is worse than the orphan it was trying to fix.
    // Tier 2 needs no reservation: the worker parents itself to the root, and the producer span
    // records with a fresh id beside it.
    nativeRequest(ROOT_TRACEPARENT);
    try {
        $method = new \ReflectionMethod(\Chronos\Collector\Framework\Laravel\RequestFacts::class, 'reservationFromPayload');
        $tierTwo = json_encode(['chronos' => ['traceparent' => ROOT_TRACEPARENT]]);
        $runner->assertTrue($method->invoke(null, $tierTwo) === null, 'the request root must not be recovered as a publish-span reservation');

        // A genuine reservation in the same request still comes back.
        $reserved = '00-'.TRACE_ID.'-99aabbccddeeff00-01';
        $recovered = $method->invoke(null, json_encode(['chronos' => ['traceparent' => $reserved]]));
        $runner->assertTrue($recovered !== null, 'a real reservation is still recovered');
        $runner->assertSame('99aabbccddeeff00', $recovered->spanId);
    } finally {
        nativeRequest(null);
    }
});

$runner->exit();

}
