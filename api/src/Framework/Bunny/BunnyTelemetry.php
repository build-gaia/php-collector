<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Bunny;

use Chronos\Collector\Dto\SpanReservation;
use Chronos\Collector\Service\Diagnostics;
use Chronos\Collector\Service\MessagingBody;
use Chronos\Collector\Service\MessagingDestination;
use Chronos\Collector\Service\MessagingFailure;
use Chronos\Collector\Service\MessagingSpan;
use Chronos\Collector\Service\MessagingWait;
use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Propagation;
use Chronos\Collector\Service\Span;
use Chronos\Collector\Service\SpanManager;
use Throwable;

/**
 * The two halves of a raw AMQP message, joined into one trace — the bunny/bunny
 * counterpart of Laravel's QueueTelemetry and Messenger's ChronosMiddleware, for
 * the same underlying reason and against a transport that gives no help at all.
 *
 * A service that publishes over `bunny/bunny` directly has no framework seam to
 * hook: there is no `createPayloadUsing`, no envelope, no stamp, no event. The
 * publish is a method call on a channel and the consume is a closure the event
 * loop invokes. So a trace stops dead at the publish — the request that caused
 * the message ends, minutes later another service does the work, and nothing
 * joins the two. Everything downstream of a message crossing a broker (the
 * queries it ran, the HTTP calls it made, the exception it swallowed) is work
 * the publishing request caused, and today it is unattributable.
 *
 * One class, two lifecycles, told apart by a fact the transport itself supplies —
 * a Channel being published to versus a Message being delivered — the same shape
 * Messenger's middleware uses with `ReceivedStamp`:
 *
 *   PUBLISH ([`publish`]): inject W3C context into the AMQP application-header
 *   table, make the real call, then record a producer span through the same
 *   `MessagingSpan` Laravel broadcasting and Messenger use, so the two halves of
 *   a stream join the Data Sources producer graph on identical `messaging.*`
 *   keys whatever the framework. Recorded AFTER the send, never before, for
 *   MessagingSpan's own stated reason: it is zero-duration by construction and
 *   fires from the FACT that the send happened.
 *
 *   CONSUME ([`consumer`]): a message-scoped request, continuing the traceparent
 *   the publish half wrote into the headers. Same trade as QueueTelemetry — the
 *   delivery IS the unit a consumer serves, with a beginning, an outcome and its
 *   own queries, so it reuses the request machinery ('QUEUE' and the queue name
 *   standing in for method and route) rather than inventing a parallel lifecycle
 *   only AMQP consumers use.
 *
 * Not split into a BunnyPublisher and a BunnyConsumer on purpose. The shared
 * vocabulary — the header names, the system constant, the destination rule — is
 * the whole reason the two halves join, and splitting it across two files is an
 * invitation for one side to start spelling a key differently from the other.
 *
 * ## The CLI flag is not needed, and leaving it off is better
 *
 * `CHRONOS_PHP_CLI_ENABLED` gates only the extension's AUTOMATIC request-start
 * in RINIT; `chronos_request_start` never consults it, so the explicit per-message
 * start below works on a consumer process with the flag unset. Leaving it OFF is
 * the better configuration rather than merely an acceptable one, for the reason
 * QueueTelemetry documents: with it on, RINIT opens a request when the consumer
 * PROCESS starts, `chronos_request_start` then ENRICHES that already-open request
 * instead of opening a new one, so the first message of every consumer would
 * inherit a trace containing everything since boot, and an idle consumer would
 * accumulate spans toward the 32,768-span ceiling with no message to attribute
 * them to.
 *
 * ## Zero runtime dependencies
 *
 * bunny/bunny is NOT a dependency of this package (the constraint in
 * composer.json is absolute; it is a `suggest` and nothing more), so — same
 * technique as Messenger's ChronosMiddleware and Monolog's ChronosHandler — the
 * whole class declaration sits behind a `class_exists()` guard. Strictly, a
 * parameter typehint resolves lazily and would not fatal on its own; the guard
 * is kept regardless for three reasons. It matches the register of every other
 * bridge here, so a reader learns one rule instead of auditing each file. Real
 * Bunny typehints mean a wrong object is caught at the call site rather than
 * silently swallowed by a `method_exists()` probe. And it makes
 * `class_exists(BunnyTelemetry::class)` a truthful wiring probe for an
 * application that wants to ask. It is also exactly why this class only ever
 * loads for an application whose own code names it: nothing inside this package
 * references it.
 *
 * ## The integration's own call sites are wired separately
 *
 * Replacing `$channel->publish(...)` with `BunnyTelemetry::publish($channel, ...)`
 * and wrapping a consume callback happen in the application's publisher and
 * consumer services, which are integration-owned — the same boundary
 * ChronosMiddleware's docblock draws around `services.yaml`/`framework.yaml`. See
 * the wiring instructions returned alongside this file; they are not edited here.
 */
if (class_exists(\Bunny\Channel::class) && class_exists(\Bunny\Message::class)) {
    final class BunnyTelemetry
    {
        /**
         * `messaging.system`. The broker, not the client library: `bunny` is how
         * this process speaks AMQP, where `rabbitmq` is what the other side of
         * the stream is — and a Go producer on the same queue says `rabbitmq`
         * too, which is what lets the two join.
         */
        public const SYSTEM = 'rabbitmq';

        public const TRACEPARENT_HEADER = 'traceparent';

        /**
         * The wall-clock instant of the publish, in the application header
         * table. Deliberately NOT the AMQP `timestamp` property: that is a
         * reserved, second-granularity field the application may want for its
         * own meaning, and a queue wait needs sub-second resolution.
         */
        public const ENQUEUED_AT_HEADER = 'x-chronos-enqueued-at';

        /**
         * Publish a message, with trace context on the wire and a producer span
         * behind it.
         *
         * The first six parameters are Bunny's `publish()` signature verbatim
         * (Channel.php:436), so an integration's diff is
         * `$channel->publish(...)` -> `BunnyTelemetry::publish($channel, ...)`
         * and nothing about the call itself changes — no reordering, no wrapper
         * object, no behaviour to re-verify.
         *
         * The four trailing parameters are exactly the facts Bunny cannot
         * supply. `AbstractClient::$options` is protected with no getter, so
         * neither the vhost nor the host is reachable from a Channel; the vhost
         * is the namespace the desktop joins on, so it has to be passed in.
         * `$messageName` is the DTO's class, which only the caller holds, and
         * `$contentType` is what the publisher declares rather than what a
         * sniffer guesses.
         *
         * Returns whatever the channel returned, untouched — `int`, `bool` or a
         * `PromiseInterface` depending on confirm mode. The publish itself is
         * outside every try/catch here: a span that fails to record must never
         * be mistaken for a send that failed.
         *
         * @param array<string, mixed> $headers the AMQP application header table
         */
        public static function publish(
            \Bunny\Channel $channel,
            // Deliberately untyped, matching Bunny's own untyped
            // `Channel::publish($body, ...)`. Under `declare(strict_types=1)` a
            // `string` hint here would reject an int or a Stringable that Bunny
            // itself accepts, so a wrapper meant to be a drop-in substitution
            // would throw where the original call worked. The span side narrows
            // it instead — see $payload below.
            mixed $body,
            array $headers = [],
            string $exchange = '',
            string $routingKey = '',
            bool $mandatory = false,
            bool $immediate = false,
            string $vhost = '',
            string $messageName = '',
            string $contentType = '',
            string $server = '',
        ): mixed {
            // The publish span's id is minted HERE, before anything reaches the
            // broker, and recorded under that same id after the send returns.
            // That ordering is the whole fix: the header has to be on the wire
            // before the publish, while the span may only be recorded once the
            // publish has happened, and the id is what bridges the two instants.
            // Until this existed the wire carried an id nothing was ever recorded
            // under, so every consumer of this message was an orphan.
            //
            // If $channel->publish() throws below, the reservation is simply
            // never spent: nothing reached the broker, so no consumer can
            // reference it.
            $reservation = SpanManager::reserve();

            // Union, not array_merge and not a merge in the other direction: a
            // caller that already set its own `traceparent` WINS. Instrumentation
            // silently rewriting an application's own propagation would be the
            // worst kind of bug — it would look like a working trace.
            $headers = $headers + self::contextHeaders($reservation);

            // Declaring a content type to the span but not to the BROKER would
            // leave the two halves of every stream disagreeing: the publish span
            // would say protobuf and the consume span, which can only read what
            // arrived, would say nothing. `content-type` is one of the reserved
            // AMQP property names ContentHeaderFrame::fromArray() lifts out of the
            // header table into a real property, so this sets the message's actual
            // content type rather than adding an application header. Union again,
            // so a caller that set it itself still wins.
            if ($contentType !== '' && !isset($headers['content-type'])) {
                $headers['content-type'] = $contentType;
            }

            $result = $channel->publish($body, $headers, $exchange, $routingKey, $mandatory, $immediate);

            try {
                // Narrowed here rather than at the signature. A non-stringable
                // body describes nothing, so it contributes no size and no
                // payload instead of guessing at one.
                $payload = \is_string($body)
                    ? $body
                    : ((\is_scalar($body) || $body instanceof \Stringable) ? (string) $body : '');

                $destination = MessagingDestination::forAmqp($vhost, $exchange, $routingKey);
                $extra = $destination;
                unset($extra[MessagingDestination::NAME]);
                $extra['messaging.protocol'] = self::protocol($contentType);
                // Size is emitted whatever the capture gate says, and measured on
                // the RAW wire bytes before any cap or base64: it is free, and it
                // is the one payload fact that survives capture being off.
                $extra['messaging.message.body.size'] = (string) \strlen($payload);
                $extra['messaging.message.body.content_type'] = $contentType;
                $extra['server.address'] = trim($server);
                // Span::MAX_TEXT_LENGTH is the real ceiling on this side, so the
                // truncation happens in PHP where `.truncated` can be set
                // honestly rather than in Span::cap() where it cannot.
                $extra += MessagingBody::encode($payload, Span::MAX_TEXT_LENGTH);

                // The same payload cut only to the operator's allowance, for the
                // span-body store. `['', '']` when capture is off, when the
                // allowance adds nothing over the preview above, or when there is
                // no payload — so the common case hands over nothing at all.
                [$whole, $wholeEncoding] = MessagingBody::whole($payload, Span::MAX_TEXT_LENGTH);

                MessagingSpan::published(
                    self::SYSTEM,
                    $destination[MessagingDestination::NAME] ?? '',
                    $messageName,
                    $extra,
                    $reservation,
                    $whole,
                    $wholeEncoding,
                );
            } catch (Throwable $error) {
                // The message is already gone. Nothing that happens while
                // describing it may reach the caller.
                //
                // Announced once per process, though: the send SUCCEEDED and its
                // producer span was lost, so the trace is missing exactly the
                // half that makes it a topology edge — and because the consumer
                // is now parented to the reserved id, it is an orphan again. An
                // operator chasing "the publish side never appears" would
                // otherwise have nothing at all to go on.
                self::warn(
                    'bunny.publish-span',
                    'a RabbitMQ publish succeeded but its producer span could not be '
                    .'recorded, so its consumer will appear unparented: '.$error->getMessage(),
                );
            }

            return $result;
        }

        /**
         * Wrap a deliver callback so each message it receives becomes its own
         * traced request.
         *
         * Called ONCE, at subscribe time, not per message:
         * `$channel->consume(BunnyTelemetry::consumer($handler, $queue, $vhost), $queue)`.
         *
         * The returned closure is Bunny's own deliver-callback shape. Channel.php:743
         * invokes `$callback($message, $this, $this->client)` with three
         * arguments while application callbacks typically declare one; PHP
         * discards extra arguments to a userland function, so the wrapper accepts
         * `...$rest` and forwards all of them, and a one-parameter application
         * closure keeps working unchanged.
         *
         * It must be the OUTERMOST wrap point. Wrapped at the callback, the ack
         * and the payload decode happen inside the traced request where they
         * belong; wrapped deeper (inside a `processMessage()`, say) the ack falls
         * outside the trace and the decode time is lost. Double-wrapping degrades
         * safely rather than nesting requests — the inner wrap hits the
         * already-active guard below and passes straight through — but it is the
         * outer one that carries the information.
         *
         * `$suppressNative` is a parameter and not a constant because span
         * OWNERSHIP is the enclosing integration's decision, not this bridge's.
         * The default matches QueueTelemetry::openJob, which is right for the
         * common case (a consumer running as an artisan command inside a Laravel
         * app, where ChronosServiceProvider has already installed the SQL hooks
         * and the decorated cache pools, so the native fallbacks would only
         * double-emit). A plain-PHP consumer with no userland SQL instrumentation
         * passes `[]` and keeps the native fallback rather than losing its
         * queries silently.
         *
         * @param callable     $handler        the application's own deliver callback
         * @param list<string> $suppressNative data-access kinds userland owns for the message
         */
        public static function consumer(
            callable $handler,
            string $queue,
            string $vhost = '',
            string $server = '',
            array $suppressNative = ['sql', 'cache'],
        ): callable {
            return static function (\Bunny\Message $message, mixed ...$rest) use (
                $handler,
                $queue,
                $vhost,
                $server,
                $suppressNative,
            ): mixed {
                // The very first statement, before any other work, so the queue
                // wait is not inflated by the cost of measuring it.
                $startedAt = \microtime(true);

                // Deciding whether to trace a message must never stop the message
                // being handled, so the WHOLE decision — the switches, the header
                // reads and requestStart itself — is fail-open.
                //
                // requestStart is the reason this needs a catch rather than trust:
                // it calls chronos_request_start() FIRST and only afterwards resets
                // the span stack and loads the instrumentation manifest, and a
                // manifest is an operator-supplied PHP file that `require` can throw
                // on. A throw there would escape into Bunny's event loop (killing a
                // consumer that should have degraded to untraced) AND leave a native
                // request open, which is the worse half: the next messages would be
                // swallowed into that one request's trace until the process died.
                //
                // `$name` is seeded with the queue so the recovery path below can
                // close a request that was opened before routeName() ever ran.
                $name = $queue;
                $opened = false;
                try {
                    // The process-level master switch, and the cheapest possible
                    // exit: a fleet image with the .so baked in but
                    // CHRONOS_PHP_ENABLED unset pays one memoised bool per message.
                    //
                    // NativeExtension::active() covers the case where a request is
                    // ALREADY open, so this delivery is being pumped from inside
                    // something already traced — a `$channel->get()` during a web
                    // request, or a nested run of the event loop. requestStart here
                    // would hijack that live request exactly as
                    // ChronosMiddleware::handleConsume describes for the sync
                    // transport. The work is already inside a trace.
                    if (NativeExtension::enabled() && !NativeExtension::active()) {
                        $opened = self::openMessage($message, $queue, $name);
                    }
                } catch (Throwable $error) {
                    // Announced once per process, and it is the most expensive
                    // silent failure in this file: a throw part-way through
                    // requestStart can leave a native request OPEN, and every
                    // later delivery on this worker is then swallowed into that
                    // one trace until the process dies. An operator seeing one
                    // enormous trace containing thousands of unrelated messages
                    // has no other clue where it came from.
                    self::warn(
                        'bunny.open-message',
                        'opening a traced request for a RabbitMQ delivery failed; later '
                        .'deliveries may be folded into one trace: '.$error->getMessage(),
                    );
                    try {
                        if (NativeExtension::active()) {
                            NativeExtension::requestEnd(0, $name);
                        }
                    } catch (Throwable $closing) {
                        // Nothing left to try. The process is in a state this
                        // bridge cannot repair, and the message still has to run.
                        self::warn(
                            'bunny.open-message-recovery',
                            'a RabbitMQ delivery left a Chronos request open that could not be '
                            .'closed; this worker\'s traces are unreliable until it restarts: '
                            .$closing->getMessage(),
                        );
                    }
                    $opened = false;
                }

                if (!$opened) {
                    return $handler($message, ...$rest);
                }

                $enqueuedAt = self::header($message, self::ENQUEUED_AT_HEADER);

                // Cleared BEFORE the fail-open block below, not inside it. The
                // slot is a static on a worker that runs for hours, so a failure
                // nobody took would be attributed to the NEXT message — and the
                // block below can throw (consumeAttributes reads the message,
                // jobStarted crosses the FFI), which would skip the clear on
                // exactly the delivery whose instrumentation already misbehaved.
                self::resetFailure();

                try {
                    $attributes = self::consumeAttributes($message, $name, $queue, $vhost, $server, $startedAt);
                    NativeExtension::setRequestAttributes($attributes);
                    // Per-request, so it must be re-declared for every message.
                    foreach ($suppressNative as $kind) {
                        if (is_string($kind) && $kind !== '') {
                            NativeExtension::suppressNative($kind);
                        }
                    }
                    // Statics that requestStart does not clear itself (it already
                    // resets SpanManager, QueryPlan and RequestFacts). Probed on
                    // `app()` so a plain-PHP consumer never reaches for the
                    // Laravel-namespaced classes at all.
                    if (\function_exists('app')) {
                        \Chronos\Collector\Framework\Laravel\ExceptionCapture::reset();
                        \Chronos\Collector\Framework\Laravel\ChronosViewEngine::resetRequestState();
                    }
                    // AFTER requestStart, never before: the in-flight marker's
                    // whole job is to name the span that will later close it, and
                    // that span does not exist until the request is open. Timeout
                    // 0 rather than a guess — AMQP gives a consumer no per-message
                    // deadline, and a guessed one reaps messages that are still
                    // working. The honest consequence is that these in-flight rows
                    // close only when a completing span closes them.
                    NativeExtension::jobStarted($name, 0, $attributes);
                } catch (Throwable) {
                    // A message must not fail because the notes about it could
                    // not be written. The request is open either way, and the
                    // close below still runs.
                }

                try {
                    $result = $handler($message, ...$rest);
                } catch (Throwable $e) {
                    self::flushRequestFacts();
                    // $handled = true: a throwable that reaches this wrapper was
                    // caught by instrumentation and is being rethrown into
                    // Bunny's event loop, not rendered into a response.
                    NativeExtension::requestEnd(0, $name, $e, true);

                    // Rethrown untouched. Telemetry never changes what the caller
                    // sees.
                    throw $e;
                }

                self::flushRequestFacts();
                // A delivery the application caught, reported and then ACKED did
                // not succeed, and until this was read the span said it had: no
                // error.type, no error.message, isError false. The handler
                // returned normally — the ack still runs, propagation is
                // untouched — so the only thing that changes is the span's
                // status, which is the honest description of what happened.
                //
                // $handled = true, matching ExceptionCapture's idiom: the
                // throwable never reached the transport, the application caught
                // it. That is what separates "failed and was swallowed" from
                // "failed and killed the consumer" — only the second stops a
                // queue.
                $failure = self::takeFailure();
                // Zero, not 200: a consumed message has no HTTP status, and
                // borrowing one would put a number in the column that means
                // something it does not mean.
                if ($failure !== null) {
                    NativeExtension::requestEnd(0, $name, $failure, true);
                } else {
                    NativeExtension::requestEnd(0, $name);
                }

                return $result;
            };
        }

        /**
         * Open a Chronos request for one delivery, and report whether the
         * collector accepted it.
         *
         * Split out of the wrapper so the throw-recovery around it has one thing
         * to guard rather than a dozen statements, and so `$name` is assigned by
         * reference BEFORE requestStart runs: if requestStart throws after the
         * native request is already open, the caller needs the real route name to
         * close it with, not the bare queue.
         *
         * False means the collector declined this request — unsampled, no
         * envelope, inert. There is then NO open request, so the caller must not
         * call requestEnd; it just runs the handler untraced.
         */
        private static function openMessage(
            \Bunny\Message $message,
            string $queue,
            string &$name,
        ): bool {
            $name = self::routeName($message, $queue);
            $traceparent = self::header($message, self::TRACEPARENT_HEADER);
            $tracestate = self::header($message, 'tracestate');
            $baggage = self::header($message, 'baggage');

            NativeExtension::requestStart(
                $traceparent !== '' ? $traceparent : null,
                $tracestate !== '' ? $tracestate : null,
                $baggage !== '' ? $baggage : null,
                // Neither a session id nor a DST directive crosses AMQP:
                // chronos_propagation_headers() returns only traceparent,
                // tracestate and baggage, and there is no accessor for the
                // current session id. Passing null is the honest shape — both are
                // additive later, one header and one probe each.
                null,
                null,
                // The queue is this message's "method and route": what it was, and
                // where it came from. Naming them in the HTTP fields keeps one
                // root-span vocabulary rather than a second one only AMQP
                // consumers use.
                'QUEUE',
                $name,
                // Empty service name → native falls back to
                // CHRONOS_PHP_APPLICATION, so a consumer and the web requests of
                // the same service land on ONE service map node.
                '',
            );

            return NativeExtension::active();
        }

        /**
         * Whether the Service/ siblings this bridge leans on are actually present.
         *
         * Cached class_exists, and NOT a formality. The estate installs this SDK
         * by copying api/src into each service's vendor tree, so version skew
         * ACROSS FILES is the normal state rather than an edge case — one service
         * was observed carrying this bridge with neither Diagnostics nor
         * MessagingFailure beside it. An unguarded static call would then raise
         * `Error: Class not found` from a spot outside every try/catch: after the
         * handler had already returned (so the ack never runs and the queue
         * stops), or from inside publish()'s catch AFTER the send succeeded (so
         * the application retries a message that was already delivered). Both are
         * telemetry deciding what happens to a message.
         */
        private static ?bool $diagnostics = null;

        private static ?bool $failures = null;

        /** Announce a lost span once per process, or stay silent if it cannot. */
        private static function warn(string $key, string $message): void
        {
            try {
                if (self::$diagnostics ??= class_exists(Diagnostics::class)) {
                    Diagnostics::warnOnce($key, $message);
                }
            } catch (Throwable) {
                // A diagnostic that throws is worse than a missing diagnostic.
            }
        }

        /** The throwable an application caught for this delivery, if any. */
        private static function takeFailure(): ?Throwable
        {
            try {
                if (self::$failures ??= class_exists(MessagingFailure::class)) {
                    return MessagingFailure::take();
                }
            } catch (Throwable) {
            }

            return null;
        }

        /** Clear any failure left by an earlier delivery on this worker. */
        private static function resetFailure(): void
        {
            try {
                if (self::$failures ??= class_exists(MessagingFailure::class)) {
                    MessagingFailure::reset();
                }
            } catch (Throwable) {
            }
        }

        /**
         * The W3C context and the enqueued-at stamp, as AMQP application headers.
         *
         * The RESERVED PUBLISH SPAN's traceparent, so the consumed message hangs
         * beneath the publish itself — the same shape an outbound HTTP call gets,
         * and now genuinely so. This used to call
         * `NativeExtension::childTraceparent()`, which mints an id that no span
         * is ever recorded under: the consumer parented itself to a span that did
         * not exist, so publish and consume shared a trace id and had no edge
         * between them at all.
         *
         * Three tiers, and the third is the one that makes the other two
         * trustworthy — a traceparent on the wire is a PROMISE that the span it
         * names will be recorded:
         *
         *   1. A reservation: propagate it, and `published()` records the publish
         *      span under that exact id.
         *   2. No reservation but a request IS open (a capacity-bound stack, a
         *      void top): propagate `NativeExtension::traceparent()`, the REQUEST
         *      ROOT's own. That is a legitimate parent, not a phantom — the root
         *      is always emitted for a sampled request — so the consumer nests
         *      under the publishing request rather than under the publish call.
         *      Less precise, still true.
         *   3. Neither (a scheduled command that never opened a request): no
         *      traceparent at all. The consumer roots its own trace, which is the
         *      honest outcome. An id is never fabricated.
         *
         * tracestate and baggage ride along because W3C requires a participant
         * that forwards traceparent to forward tracestate it does not understand,
         * and every HTTP bridge in this package already does.
         *
         * The enqueued-at stamp is present on every publish, including one with
         * no open request and therefore no traceparent at all — exactly as
         * QueueTelemetry::payloadContext() argues: a message published from a
         * scheduled command has no trace to continue but has waited just as long,
         * and dropping the stamp would leave that wait unmeasurable.
         *
         * Verified to survive the wire: `ContentHeaderFrame::fromArray()` moves
         * only the thirteen reserved AMQP property names out of the array into
         * properties, and none of these three is one of them, so they land in the
         * application header table and come back verbatim from
         * `ContentHeaderFrame::toArray()` on the consume side.
         *
         * @return array<string, string>
         */
        private static function contextHeaders(?SpanReservation $reservation = null): array
        {
            try {
                $headers = [self::ENQUEUED_AT_HEADER => \sprintf('%.6F', \microtime(true))];
                $traceparent = $reservation?->header() ?? NativeExtension::traceparent();
                if (is_string($traceparent) && $traceparent !== '') {
                    $headers[self::TRACEPARENT_HEADER] = $traceparent;
                }

                return $headers + Propagation::contextHeaders();
            } catch (Throwable) {
                // No context is a message that starts a fresh trace on the other
                // side. A failed publish would be very much worse.
                return [];
            }
        }

        /**
         * Root-span attributes describing the message being processed, in the
         * same `messaging.*` vocabulary the publish side writes — so the two
         * halves of one stream describe the same place in the same keys.
         *
         * The known `$queue` is passed to `forAmqp`, so NAME is the queue this
         * consumer really read from while VIA and ROUTE come off the delivery
         * itself: a consume span is never the ambiguous name-only match a topic
         * publish has to be.
         *
         * Deliberately omitted: the delivery tag and the consumer tag (both
         * per-connection, unbounded, and useless to join on) and
         * `messaging.consumer.group.name` (AMQP has no consumer groups, and
         * borrowing Kafka's word for a queue would invent a concept).
         *
         * @return array<string, string>
         */
        private static function consumeAttributes(
            \Bunny\Message $message,
            string $name,
            string $queue,
            string $vhost,
            string $server,
            float $startedAt,
        ): array {
            $attributes = [
                'span.kind' => 'consumer',
                'messaging.system' => self::SYSTEM,
                // Process, not receive: only the handler has a duration worth
                // looking at.
                'messaging.operation' => 'process',
            ];
            $attributes += MessagingDestination::forAmqp(
                $vhost,
                is_string($message->exchange ?? null) ? $message->exchange : '',
                is_string($message->routingKey ?? null) ? $message->routingKey : '',
                $queue,
            );

            // Null when the stamp is missing, unparseable, or in the future —
            // never zero, which would let an unmeasured queue report the
            // healthiest possible wait. See MessagingWait on why skew is
            // discarded rather than clamped.
            $waited = MessagingWait::milliseconds(self::header($message, self::ENQUEUED_AT_HEADER), $startedAt);
            if ($waited !== null) {
                $attributes['messaging.message.queue_time_ms'] = (string) $waited;
            }

            // AMQP's only retry signal, and the field that separates "slow" from
            // "failing and being redelivered" — which look identical without it.
            if (($message->redelivered ?? null) === true) {
                $attributes['messaging.message.redelivered'] = 'true';
            }
            $attributes['messaging.message.id'] = self::header($message, 'message-id');
            $attributes['messaging.message.conversation_id'] = self::header($message, 'correlation-id');
            // From the AMQP `type` property only. Never back-filled from the
            // queue name: a queue is a PLACE, and naming it as the message type
            // would make every message on one queue look like one type.
            $attributes['messaging.message.name'] = self::header($message, 'type');

            $contentType = self::header($message, 'content-type');
            $attributes['messaging.protocol'] = self::protocol($contentType);
            $attributes['messaging.message.body.content_type'] = $contentType;
            $attributes['server.address'] = trim($server);

            $content = is_string($message->content ?? null) ? $message->content : '';
            $attributes['messaging.message.body.size'] = (string) \strlen($content);
            // 8192, not Span::MAX_TEXT_LENGTH: a consume body rides the
            // request-attribute bag, whose native cap (request_attributes.rs
            // MAX_VALUE_BYTES) is 8 KiB. Handing over more would have it cut
            // where `.truncated` cannot be set, and an oversized body would
            // arrive looking complete.
            $attributes += MessagingBody::encode($content, 8192);
            // The consume side is where the blob store earns the most: that 8192
            // is the tightest bound anywhere in this pipeline, so a payload of
            // any size arrives as a stub. Empty ids on purpose — the blob is then
            // keyed by the CONSUMER REQUEST's ROOT span, which is the span the
            // request-attribute bag's values land on, and keeping the preview and
            // the payload on one span is the invariant the whole store rests on.
            [$whole, $wholeEncoding] = MessagingBody::whole($content, 8192);
            if ($whole !== '' && NativeExtension::storeSpanBody('', '', 'message', $contentType, $whole, $wholeEncoding)) {
                $attributes[MessagingBody::STORED] = 'true';
            }

            // setRequestAttributes drops non-scalars but not empty strings, and
            // absent-never-guessed means an empty fact is no fact.
            return array_filter($attributes, static fn (string $value): bool => $value !== '');
        }

        /**
         * One AMQP header as a trimmed string, or `''` when it is not a string
         * at all.
         *
         * The `is_string` check is not defensive padding. `Message::$headers` is
         * `ContentHeaderFrame::toArray()`, which merges the AMQP properties and
         * the nested field table into one array, so a value can legitimately be
         * an int (`delivery-mode`, `priority`), a `DateTime` (`timestamp`) or
         * another array (a nested header table) — and `getHeader()` is untyped
         * and hands back whatever is there.
         */
        private static function header(\Bunny\Message $message, string $name): string
        {
            try {
                $value = $message->getHeader($name);

                return is_string($value) ? trim($value) : '';
            } catch (Throwable) {
                return '';
            }
        }

        /**
         * The payload's wire format, as a bounded low-cardinality word.
         *
         * Only the two this estate actually sends, and only from what the
         * publisher DECLARED — never sniffed from the bytes. An unrecognised or
         * absent content type yields `''` and the attribute is dropped, which is
         * the same bounded key RequestFacts already writes as `'json'`.
         */
        private static function protocol(string $contentType): string
        {
            // Lowercased and trimmed before matching, because a media type is
            // case-insensitive per RFC 9110 §8.3.1 and both halves of one stream
            // have to agree. Without this, a publisher declaring
            // `application/X-Protobuf` — legal, and what several generators emit —
            // yields `protobuf` from one side and no attribute at all from the
            // other, which is two spellings of one fact on one stream.
            $type = strtolower(trim($contentType));
            if (str_contains($type, 'protobuf')) {
                return 'protobuf';
            }

            return str_contains($type, 'json') ? 'json' : '';
        }

        /**
         * The route pattern for this delivery's request root.
         *
         * The queue the consumer subscribed to, because it is an operator-chosen,
         * bounded string and it is what this consumer actually asked for. The
         * routing key and exchange are fallbacks for a consumer that did not name
         * its queue; a delivery tag or consumer tag would be unbounded and would
         * turn every message into its own route.
         */
        private static function routeName(\Bunny\Message $message, string $queue): string
        {
            $queue = trim($queue);
            if ($queue !== '') {
                return $queue;
            }
            $routingKey = is_string($message->routingKey ?? null) ? trim($message->routingKey) : '';
            if ($routingKey !== '') {
                return $routingKey;
            }
            $exchange = is_string($message->exchange ?? null) ? trim($message->exchange) : '';

            return $exchange !== '' ? $exchange : 'amqp';
        }

        /**
         * Close out the Laravel-side per-request bookkeeping, on both the success
         * and the failure path — mirroring QueueTelemetry::closeJob, and each in
         * its own try/catch so one failing does not skip the other or the
         * requestEnd that follows.
         */
        private static function flushRequestFacts(): void
        {
            if (!\function_exists('app')) {
                return;
            }
            try {
                \Chronos\Collector\Framework\Laravel\RichTelemetryHooks::closeDanglingTransactions();
            } catch (Throwable) {
            }
            try {
                \Chronos\Collector\Framework\Laravel\RequestFacts::flush([
                    'process.runtime.memory.peak_bytes' => (string) \memory_get_peak_usage(true),
                ]);
            } catch (Throwable) {
            }
        }
    }
}
