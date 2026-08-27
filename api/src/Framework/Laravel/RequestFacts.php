<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\ActivityCatalog;
use Chronos\Collector\Service\CallSite;
use Chronos\Collector\Service\MessagingSpan;
use Chronos\Collector\Service\NativeExtension;
use Throwable;

/**
 * Bounded framework facts stamped onto the request root span at flush.
 *
 * This is the Debugbar-shaped hydration that belongs on a trace: which action
 * ran, who was authenticated, which views/models/mail/authorization checks
 * participated — names and counts only. View data, cache values, session
 * contents, notification bodies and Gate arguments stay out.
 *
 * The attribute names are FRAMEWORK-GENERIC (`framework.views`, not
 * `laravel.views`) per ADR 0024 §1: a Symfony request renders views and hydrates
 * models too, and which framework did it is already on the span as
 * `app.framework`. Events and jobs are not counts at all any more — they are
 * catalogs (§2, §3), because a destination, a transport and a dispatch site are
 * the questions asked straight after "did it happen".
 *
 * Accumulated in userland because only Laravel knows these events; flushed
 * through the native collector because the synthetic SpanManager root is never
 * written (the .so emits the real request root at request_end).
 */
final class RequestFacts
{
    private const MAX_UNIQUE = 32;

    private const MAX_NAME = 128;

    /** @var array<string, int> */
    private static array $views = [];

    /** @var array<string, int> */
    private static array $models = [];

    /** @var array<string, int> */
    private static array $mail = [];

    /** @var array<string, int> */
    private static array $gates = [];

    private static ?ActivityCatalog $events = null;

    private static ?ActivityCatalog $jobs = null;

    private static int $droppedViews = 0;

    private static int $droppedModels = 0;

    private static int $droppedMail = 0;

    private static int $droppedGates = 0;

    public static function reset(): void
    {
        self::$views = [];
        self::$models = [];
        self::$mail = [];
        self::$gates = [];
        self::$droppedViews = 0;
        self::$droppedModels = 0;
        self::$droppedMail = 0;
        self::$droppedGates = 0;
        self::events()->reset();
        self::jobs()->reset();
    }

    public static function noteView(string $name): void
    {
        self::note(self::$views, self::$droppedViews, $name);
    }

    public static function noteModel(string $class): void
    {
        self::note(self::$models, self::$droppedModels, $class);
    }

    public static function noteMail(string $type): void
    {
        self::note(self::$mail, self::$droppedMail, $type);
    }

    /**
     * One queued job, as a catalog record.
     *
     * `transport` is the queue connection's DRIVER, not its name: "redis" says
     * where to look, where the application's own name for the connection
     * ("default") says nothing. `handlerFile` is the one field about the code
     * rather than the dispatch, and it is what turns "this request queued
     * IndexUser" into somewhere to go.
     */
    public static function noteJob(
        string $job,
        string $queue = '',
        string $transport = '',
        ?int $delayMs = null,
        string $handlerFile = '',
        ?int $payloadSize = null,
    ): void {
        if ($job === '') {
            return;
        }
        self::jobs()->record(
            $job.'@'.$queue,
            static fn (): array => [
                'name' => $job,
                'queue' => $queue,
                'transport' => $transport,
                'delay_ms' => $delayMs,
                'handler.filepath' => $handlerFile,
                'payload.size' => $payloadSize,
            ] + CallSite::attributes(),
        );
    }

    public static function noteGate(string $ability, bool $allowed): void
    {
        $label = $ability.':'.($allowed ? 'allow' : 'deny');
        self::note(self::$gates, self::$droppedGates, $label);
    }

    /**
     * One dispatched event, as a catalog record.
     *
     * `in_process` is a deliberate member of the destination vocabulary rather
     * than an absence: a Laravel event with a synchronous listener really has no
     * broker, and recording that as "no destination" would make the common case
     * look like missing data.
     */
    public static function noteEvent(
        string $name,
        string $destinationKind = 'in_process',
        string $destination = '',
        string $protocol = '',
        string $schema = '',
    ): void {
        if ($name === '' || self::isFrameworkEvent($name)) {
            return;
        }
        self::events()->record(
            $name.'@'.$destinationKind.'/'.$destination,
            static fn (): array => [
                'name' => $name,
                'destination' => $destination,
                'destination.kind' => $destinationKind,
                'operation' => $destinationKind === 'in_process' ? 'process' : 'publish',
                'protocol' => $protocol,
                'schema' => $schema === '' ? $name : $schema,
            ] + CallSite::attributes(),
        );
    }

    private static function events(): ActivityCatalog
    {
        return self::$events ??= new ActivityCatalog();
    }

    private static function jobs(): ActivityCatalog
    {
        return self::$jobs ??= new ActivityCatalog();
    }

    /**
     * Identity known only at request end: route action/name/middleware, auth id,
     * peak memory. Missing pieces are omitted rather than written empty.
     *
     * @param array<int, string> $middleware
     * @return array<string, string>
     */
    public static function identity(
        string $routeName = '',
        string $routeAction = '',
        array $middleware = [],
        string $userId = '',
        string $guard = '',
        int $peakMemoryBytes = 0,
    ): array {
        $attributes = [];
        if ($routeName !== '') {
            $attributes['http.route.name'] = self::clip($routeName, self::MAX_NAME);
        }
        if ($routeAction !== '') {
            $attributes['http.route.action'] = self::clip($routeAction, 256);
        }
        $names = [];
        foreach (array_slice($middleware, 0, 16) as $entry) {
            $trimmed = self::clip($entry, 64);
            if ($trimmed !== '') {
                $names[] = $trimmed;
            }
        }
        if ($names !== []) {
            $encoded = json_encode($names, JSON_UNESCAPED_SLASHES);
            if (is_string($encoded)) {
                $attributes['http.route.middleware'] = $encoded;
            }
        }
        if ($userId !== '') {
            $attributes['enduser.id'] = self::clip($userId, 64);
        }
        if ($guard !== '') {
            $attributes['enduser.guard'] = self::clip($guard, 32);
        }
        if ($peakMemoryBytes > 0) {
            $attributes['process.runtime.memory.peak_bytes'] = (string) $peakMemoryBytes;
        }

        return $attributes;
    }

    /**
     * Snapshot of everything observed this request, including identity if supplied.
     *
     * @param array<string, string> $identity
     * @return array<string, string>
     */
    public static function snapshot(array $identity = []): array
    {
        $attributes = $identity;
        self::putCounts($attributes, 'framework.views', self::$views, self::$droppedViews);
        self::putCounts($attributes, 'framework.models', self::$models, self::$droppedModels);
        self::putCounts($attributes, 'framework.mail', self::$mail, self::$droppedMail);
        self::putCounts($attributes, 'framework.authorization', self::$gates, self::$droppedGates);
        self::events()->putInto($attributes, 'messaging.events');
        self::jobs()->putInto($attributes, 'messaging.jobs');

        return $attributes;
    }

    /** Push the snapshot onto the native request root and clear for the next request. */
    public static function flush(array $identity = []): void
    {
        try {
            $attributes = self::snapshot($identity);
            if ($attributes !== []) {
                NativeExtension::setRequestAttributes($attributes);
            }
        } catch (Throwable) {
        }
        self::reset();
    }

    /**
     * Subscribe to Laravel events that hydrate the request root. Names and counts
     * only; payloads, view data and Gate arguments are never read.
     */
    public static function listen(): void
    {
        if (!class_exists(\Illuminate\Support\Facades\Event::class)) {
            return;
        }
        try {
            $event = \Illuminate\Support\Facades\Event::class;
            if (class_exists(\Illuminate\Database\Eloquent\Events\Retrieved::class)) {
                $event::listen(\Illuminate\Database\Eloquent\Events\Retrieved::class, static function (object $observed): void {
                    $model = $observed->model ?? null;
                    if (is_object($model)) {
                        self::noteModel($model::class);
                    }
                });
            }
            $event::listen('composing:*', static function (string $name, array $payload = []): void {
                $view = $payload[0] ?? null;
                $viewName = is_object($view) && method_exists($view, 'name') ? (string) $view->name() : '';
                if ($viewName === '' && str_starts_with($name, 'composing: ')) {
                    $viewName = substr($name, 11);
                }
                if ($viewName !== '') {
                    self::noteView($viewName);
                }
            });
            if (class_exists(\Illuminate\Mail\Events\MessageSent::class)) {
                $event::listen(\Illuminate\Mail\Events\MessageSent::class, static function (object $observed): void {
                    self::noteMail(self::mailType($observed));
                });
            }
            if (class_exists(\Illuminate\Notifications\Events\NotificationSent::class)) {
                $event::listen(\Illuminate\Notifications\Events\NotificationSent::class, static function (object $observed): void {
                    $notification = $observed->notification ?? null;
                    if (is_object($notification)) {
                        self::noteMail($notification::class);
                    }
                });
            }
            if (class_exists(\Illuminate\Queue\Events\JobQueued::class)) {
                $event::listen(\Illuminate\Queue\Events\JobQueued::class, static function (object $observed): void {
                    $job = $observed->job ?? null;
                    $name = is_object($job) ? $job::class : (is_string($job) ? $job : '');
                    if ($name === '') {
                        return;
                    }
                    $queue = isset($observed->queue) && is_string($observed->queue) ? $observed->queue : '';
                    $transport = self::queueDriver($observed->connectionName ?? null);
                    $payloadSize = self::payloadSize($observed->payload ?? null);
                    self::noteJob(
                        $name,
                        $queue,
                        $transport,
                        self::jobDelayMs(is_object($job) ? $job : null),
                        is_object($job) ? self::classFile($job::class) : '',
                        $payloadSize,
                    );
                    // A job pushed onto redis/sqs/a database really leaves the
                    // process and will run in another trace, so it is an edge and
                    // gets a tier-2 span. The `sync` driver runs it inline: no
                    // boundary crossed, nothing to draw.
                    if ($transport !== '' && $transport !== 'sync') {
                        MessagingSpan::published($transport, $queue, $name, array_filter([
                            'messaging.message.body.size' => $payloadSize === null ? '' : (string) $payloadSize,
                        ]));
                    }
                });
            }
            if (class_exists(\Illuminate\Auth\Access\Events\GateEvaluated::class)) {
                $event::listen(\Illuminate\Auth\Access\Events\GateEvaluated::class, static function (object $observed): void {
                    $ability = isset($observed->ability) && is_string($observed->ability) ? $observed->ability : '';
                    if ($ability === '') {
                        return;
                    }
                    $allowed = $observed->result ?? false;
                    self::noteGate($ability, $allowed === true);
                });
            }
            $event::listen('*', static function (mixed ...$args): void {
                $observed = $args[0] ?? '';
                // The framework filter runs FIRST. This closure fires for every
                // internal Illuminate event, and interrogating each one — a
                // broadcastOn() call, a backtrace — would put the collector's
                // cost on work it then throws away.
                $name = is_object($observed) ? $observed::class : (is_string($observed) ? $observed : '');
                if ($name === '' || self::isFrameworkEvent($name)) {
                    return;
                }
                if (is_string($observed)) {
                    // A string event name has no object to interrogate, so the
                    // most that can be said is that it was dispatched in-process.
                    self::noteEvent($observed);

                    return;
                }
                // Broadcasting is what makes an event LEAVE the process. Everything
                // else is a synchronous listener call, however many of them there
                // are, and calling that `in_process` is the accurate reading.
                if (!self::isBroadcast($observed)) {
                    self::noteEvent($name);

                    return;
                }
                $driver = self::broadcastDriver();
                $destination = self::broadcastDestination($observed);
                self::noteEvent($name, $driver, $destination, 'json');
                // Tier 2 (ADR 0024 §2): this message really leaves the process,
                // so it is a topology edge and gets its own span carrying the
                // OTel messaging.* keys the producer graph joins on.
                MessagingSpan::published($driver, $destination, $name, ['messaging.protocol' => 'json']);
            });
        } catch (Throwable) {
        }
    }

    /**
     * Whether the event is broadcast, and therefore actually crosses a process
     * boundary. `ShouldBroadcastNow` extends `ShouldBroadcast`, so one check
     * covers both.
     */
    private static function isBroadcast(object $event): bool
    {
        return interface_exists(\Illuminate\Contracts\Broadcasting\ShouldBroadcast::class)
            && $event instanceof \Illuminate\Contracts\Broadcasting\ShouldBroadcast;
    }

    /**
     * The broadcast transport's own name — `redis`, `pusher`, `ably`, `log`.
     *
     * Deliberately not folded into a closed vocabulary: a destination kind that
     * cannot say "pusher" would have to say something false instead, and the
     * point of the field is to name where the message went. `in_process` is the
     * one reserved member, because it is the one case with no transport at all.
     */
    private static function broadcastDriver(): string
    {
        try {
            if (!function_exists('config')) {
                return 'broadcast';
            }
            $connection = config('broadcasting.default');
            if (!is_string($connection) || $connection === '') {
                return 'broadcast';
            }
            $driver = config("broadcasting.connections.{$connection}.driver");

            return is_string($driver) && $driver !== '' ? $driver : $connection;
        } catch (Throwable) {
            return 'broadcast';
        }
    }

    /**
     * The channels the event was broadcast on, comma-joined.
     *
     * `broadcastOn()` is application code and is called here — unavoidably, since
     * it is the only place the channel names exist. It is the one application
     * method this class invokes, it is conventionally a pure `return new
     * Channel(...)`, and it is wrapped: a throwing implementation costs the
     * destination field and nothing else.
     *
     * `broadcastWith()` is NOT called, which is why no `payload.size` is recorded
     * for an event. It builds the payload rather than reporting it, so calling it
     * would run application work a second time and risk doubling whatever side
     * effect it has.
     */
    private static function broadcastDestination(object $event): string
    {
        try {
            if (!method_exists($event, 'broadcastOn')) {
                return '';
            }
            $channels = $event->broadcastOn();
            if (!is_array($channels)) {
                $channels = [$channels];
            }
            $names = [];
            foreach (array_slice($channels, 0, 4) as $channel) {
                $name = match (true) {
                    is_string($channel) => $channel,
                    is_object($channel) && property_exists($channel, 'name') && is_string($channel->name) => $channel->name,
                    is_object($channel) && method_exists($channel, '__toString') => (string) $channel,
                    default => '',
                };
                if ($name !== '') {
                    $names[] = $name;
                }
            }

            return implode(',', $names);
        } catch (Throwable) {
            return '';
        }
    }

    /** The queue connection's driver, which is where to look; the name is not. */
    private static function queueDriver(mixed $connectionName): string
    {
        try {
            if (!is_string($connectionName) || $connectionName === '' || !function_exists('config')) {
                return is_string($connectionName) ? $connectionName : '';
            }
            $driver = config("queue.connections.{$connectionName}.driver");

            return is_string($driver) && $driver !== '' ? $driver : $connectionName;
        } catch (Throwable) {
            return '';
        }
    }

    /**
     * The dispatch delay in milliseconds, when one was set. Laravel accepts an
     * int of seconds, a DateInterval or an absolute DateTimeInterface, so all
     * three are resolved to the same unit rather than reported in whichever one
     * the caller happened to use.
     */
    private static function jobDelayMs(?object $job): ?int
    {
        try {
            if ($job === null || !property_exists($job, 'delay')) {
                return null;
            }
            $delay = $job->delay;

            return match (true) {
                is_int($delay) || is_float($delay) => (int) ($delay * 1000),
                $delay instanceof \DateInterval => (int) (((float) $delay->format('%a')) * 86400000)
                    + ($delay->h * 3600000) + ($delay->i * 60000) + ($delay->s * 1000),
                $delay instanceof \DateTimeInterface => max(0, (int) (($delay->getTimestamp() - time()) * 1000)),
                default => null,
            };
        } catch (Throwable) {
            return null;
        }
    }

    /** The file a class is declared in, for the jump-to-handler link. */
    private static function classFile(string $class): string
    {
        try {
            if (!class_exists($class)) {
                return '';
            }
            $file = (new \ReflectionClass($class))->getFileName();

            return is_string($file) ? $file : '';
        } catch (Throwable) {
            return '';
        }
    }

    /**
     * The encoded size of an already-serialised payload.
     *
     * Read only, never re-encoded: the size is evidence about a payload that
     * exists, not a reason to build one.
     */
    private static function payloadSize(mixed $payload): ?int
    {
        if (is_string($payload) && $payload !== '') {
            return strlen($payload);
        }

        return null;
    }

    private static function mailType(object $event): string
    {
        $data = is_array($event->data ?? null) ? $event->data : [];
        foreach (['__laravel_notification', '__laravel_notification_class'] as $key) {
            if (isset($data[$key]) && is_string($data[$key]) && $data[$key] !== '') {
                return $data[$key];
            }
        }

        return 'mail';
    }

    /** @param array<string, int> $bucket */
    private static function note(array &$bucket, int &$dropped, string $name): void
    {
        $name = self::clip($name, self::MAX_NAME);
        if ($name === '') {
            return;
        }
        if (isset($bucket[$name])) {
            ++$bucket[$name];

            return;
        }
        if (count($bucket) >= self::MAX_UNIQUE) {
            ++$dropped;

            return;
        }
        $bucket[$name] = 1;
    }

    /**
     * @param array<string, string> $attributes
     * @param array<string, int>    $bucket
     */
    private static function putCounts(array &$attributes, string $key, array $bucket, int $dropped): void
    {
        if ($bucket === []) {
            return;
        }
        arsort($bucket);
        $encoded = json_encode($bucket, JSON_UNESCAPED_SLASHES);
        if (!is_string($encoded)) {
            return;
        }
        $attributes[$key] = $encoded;
        if ($dropped > 0) {
            $attributes[$key.'.truncated'] = 'true';
        }
    }

    private static function isFrameworkEvent(string $name): bool
    {
        if ($name === '' || str_starts_with($name, 'Illuminate\\') || str_starts_with($name, 'eloquent.')) {
            return true;
        }
        foreach (['composing:', 'composed:', 'creating:', 'bootstrapped:', 'booting:'] as $prefix) {
            if (str_starts_with($name, $prefix)) {
                return true;
            }
        }

        return str_starts_with($name, 'Chronos\\');
    }

    private static function clip(string $value, int $max): string
    {
        $value = trim($value);

        return strlen($value) > $max ? substr($value, 0, $max) : $value;
    }
}
