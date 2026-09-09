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
 * participated. View data, cache values, session contents, notification bodies
 * and Gate argument *values* stay out. Target classes and argument types are
 * cheap and in.
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

    /** @var array<string, int> */
    private static array $modelWrites = [];

    /** @var array<string, int> */
    private static array $exceptions = [];

    private static ?ActivityCatalog $events = null;

    private static ?ActivityCatalog $jobs = null;

    private static ?ActivityCatalog $authorizations = null;

    private static ?ActivityCatalog $viewTemplates = null;

    private static int $droppedViews = 0;

    private static int $droppedModels = 0;

    private static int $droppedMail = 0;

    private static int $droppedGates = 0;

    private static int $droppedModelWrites = 0;

    private static int $droppedExceptions = 0;

    public static function reset(): void
    {
        self::$views = [];
        self::$models = [];
        self::$mail = [];
        self::$gates = [];
        self::$modelWrites = [];
        self::$exceptions = [];
        self::$droppedModelWrites = 0;
        self::$droppedExceptions = 0;
        self::$droppedViews = 0;
        self::$droppedModels = 0;
        self::$droppedMail = 0;
        self::$droppedGates = 0;
        self::events()->reset();
        self::jobs()->reset();
        self::authorizations()->reset();
        self::viewTemplates()->reset();
    }

    /**
     * One rendered template, with the file it was compiled from when known.
     *
     * The path is what turns a row naming `errors::403` into somewhere to go. It
     * comes from the View object the framework hands the `composing:` listener and
     * is never resolved through the view finder here: finding a template is a
     * filesystem search the framework has already done, and doing it again would
     * put that cost on every render.
     */
    public static function noteView(string $name, string $path = ''): void
    {
        self::note(self::$views, self::$droppedViews, $name);
        if ($path === '') {
            return;
        }
        self::viewTemplates()->record(
            $name,
            static fn (): array => ['name' => $name, 'code.filepath' => $path],
        );
    }

    /**
     * A view the framework just started composing.
     *
     * When the view is Inertia's root, the Blade name is the layout and the page
     * the author wrote (`Profile/Edit`) lives on the view data. Prefer that.
     */
    public static function noteComposedView(string $eventName, mixed $view = null): void
    {
        $viewName = is_object($view) && method_exists($view, 'name') ? (string) $view->name() : '';
        if ($viewName === '' && str_starts_with($eventName, 'composing: ')) {
            $viewName = substr($eventName, 11);
        }
        $component = self::inertiaComponent($view);
        if ($component !== '') {
            self::noteView($component);

            return;
        }
        if ($viewName !== '') {
            self::noteView($viewName, self::viewPath($view));
        }
    }

    /** The compiled template's own file, as the View object reports it. */
    private static function viewPath(mixed $view): string
    {
        try {
            if (!is_object($view) || !method_exists($view, 'getPath')) {
                return '';
            }
            $path = $view->getPath();

            return is_string($path) ? $path : '';
        } catch (Throwable) {
            return '';
        }
    }

    public static function noteModel(string $class): void
    {
        if ($class === '') {
            return;
        }
        self::note(self::$models, self::$droppedModels, $class);
    }

    public static function noteMail(string $type): void
    {
        self::note(self::$mail, self::$droppedMail, $type);
    }

    /**
     * One Eloquent write, as `App\\Models\\User:created`.
     *
     * Kept apart from `framework.models` rather than folded into it, because the
     * two answer different questions. That map counts HYDRATION — how many rows
     * this request turned into objects, which is the N+1 question. This one counts
     * PERSISTENCE, which is the "what did this request change" question, and a
     * single number covering both would answer neither.
     */
    public static function noteModelWrite(string $class, string $operation): void
    {
        if ($class === '' || $operation === '') {
            return;
        }
        self::note(self::$modelWrites, self::$droppedModelWrites, $class.':'.$operation);
    }

    /**
     * One reported exception, by class.
     *
     * The spans ExceptionCapture emits carry the stack and the position in time;
     * this count is what makes a recovered failure visible on the root without
     * opening the trace, so a request that swallowed six timeouts does not read
     * like one that did nothing.
     */
    public static function noteException(string $class): void
    {
        self::note(self::$exceptions, self::$droppedExceptions, $class);
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

    /**
     * One authorization check.
     *
     * The count key stays `ability:allow` when nothing else is known, so existing
     * traces keep their shape. A target — the first argument's class, which is
     * cheap and not the object's contents — is appended as `ability@User:allow`,
     * which is what turns "a gate named accessBackoffice" into Debugbar's
     * `accessBackoffice App\Models\User`. Argument *values* stay out of the count
     * key; a cheap type/class summary lives on the catalog record instead.
     *
     * The catalog record also names the policy method that returned the verdict
     * and the file it lives in, so a `deny` is one click from the code that said
     * so. That resolution sits inside the catalog's field callable, which means it
     * is paid once per distinct check and never for a repeat.
     */
    public static function noteGate(
        string $ability,
        bool $allowed,
        string $target = '',
        string $argumentSummary = '',
    ): void {
        if ($ability === '') {
            return;
        }
        $result = $allowed ? 'allow' : 'deny';
        $shortTarget = self::shortClass($target);
        $label = $shortTarget === '' ? $ability.':'.$result : $ability.'@'.$shortTarget.':'.$result;
        self::note(self::$gates, self::$droppedGates, $label);
        self::authorizations()->record(
            $label,
            static fn (): array => [
                'name' => $ability,
                'result' => $result,
                'target' => $target,
                'arguments' => $argumentSummary,
            ] + self::policySite($ability, $target),
        );
    }

    /**
     * Where the verdict was decided: `App\Policies\MessagePolicy::manageMessages`
     * and its file and line.
     *
     * Resolution is by REFLECTION only — `getPolicyFor()` returns the registered
     * policy instance without evaluating anything, and a `ReflectionMethod` reads
     * the declaration. Nothing here calls the ability. A closure-defined ability is
     * reflected the same way, and a gate defined by neither resolves to no fields
     * rather than to a guess.
     *
     * @return array<string, string>
     */
    private static function policySite(string $ability, string $target): array
    {
        try {
            if (!class_exists(\Illuminate\Support\Facades\Gate::class)) {
                return [];
            }
            $gate = \Illuminate\Support\Facades\Gate::class;
            if ($target !== '') {
                $policy = $gate::getPolicyFor($target);
                if (is_object($policy) && method_exists($policy, $ability)) {
                    return self::declaration(
                        $policy::class.'::'.$ability,
                        new \ReflectionMethod($policy, $ability),
                    );
                }
            }
            $abilities = $gate::abilities();
            $callback = is_array($abilities) ? ($abilities[$ability] ?? null) : null;
            if ($callback instanceof \Closure) {
                return self::declaration($ability, new \ReflectionFunction($callback));
            }
            if (is_string($callback) && str_contains($callback, '@')) {
                [$class, $method] = explode('@', $callback, 2);
                if (class_exists($class) && method_exists($class, $method)) {
                    return self::declaration(
                        $class.'::'.$method,
                        new \ReflectionMethod($class, $method),
                    );
                }
            }
        } catch (Throwable) {
        }

        return [];
    }

    /**
     * @return array<string, string>
     */
    private static function declaration(string $policy, \ReflectionFunctionAbstract $function): array
    {
        $fields = ['policy' => $policy];
        $file = $function->getFileName();
        if (is_string($file) && $file !== '') {
            $fields['code.filepath'] = $file;
            $fields['code.lineno'] = (string) $function->getStartLine();
        }

        return $fields;
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

    private static function authorizations(): ActivityCatalog
    {
        return self::$authorizations ??= new ActivityCatalog();
    }

    private static function viewTemplates(): ActivityCatalog
    {
        return self::$viewTemplates ??= new ActivityCatalog();
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
        self::putCounts($attributes, 'framework.model.writes', self::$modelWrites, self::$droppedModelWrites);
        self::putCounts($attributes, 'framework.exceptions', self::$exceptions, self::$droppedExceptions);
        self::events()->putInto($attributes, 'messaging.events');
        self::jobs()->putInto($attributes, 'messaging.jobs');
        self::authorizations()->putInto($attributes, 'framework.authorization.checks');
        self::viewTemplates()->putInto($attributes, 'framework.views.templates');

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
     * Subscribe to Laravel events that hydrate the request root. Names, counts,
     * target classes and argument types; payloads, view data and Gate argument
     * values are never read.
     */
    public static function listen(): void
    {
        if (!class_exists(\Illuminate\Support\Facades\Event::class)) {
            return;
        }
        try {
            $event = \Illuminate\Support\Facades\Event::class;
            // The STRING events, not the Events\Retrieved class.
            //
            // Eloquent only dispatches its class-based events for a model that
            // declares $dispatchesEvents for that hook; every ordinary model
            // dispatches `eloquent.retrieved: App\Models\User` and nothing else
            // (HasEvents::fireModelEvent). Listening for the class therefore heard
            // nothing at all from a normal application, which is why this map
            // could be empty on a request that hydrated thousands of rows.
            //
            // Only the string form is subscribed: a model that DOES declare
            // $dispatchesEvents fires the class event and, when its listener
            // returns nothing, the string event as well — so listening for both
            // would count those models twice.
            $event::listen('eloquent.retrieved: *', static function (string $name, array $payload = []): void {
                self::noteModel(self::modelClassFromEvent($name, $payload));
            });
            foreach (['created', 'updated', 'deleted'] as $operation) {
                $event::listen(
                    'eloquent.'.$operation.': *',
                    static function (string $name, array $payload = []) use ($operation): void {
                        self::noteModelWrite(self::modelClassFromEvent($name, $payload), $operation);
                    },
                );
            }
            $event::listen('composing:*', static function (string $name, array $payload = []): void {
                self::noteComposedView($name, $payload[0] ?? null);
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
                    $arguments = is_array($observed->arguments ?? null) ? $observed->arguments : [];
                    self::noteGate(
                        $ability,
                        $allowed === true,
                        self::gateTarget($arguments),
                        self::gateArgumentSummary($arguments),
                    );
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
     * Inertia's page component, when this view is the Inertia root.
     *
     * The Blade name is the layout (`app`); the component is the page
     * (`Profile/Edit`). Reading it from view data is cheap — a string already
     * on the view — and does not execute application code.
     */
    private static function inertiaComponent(mixed $view): string
    {
        if (!is_object($view) || !method_exists($view, 'getData')) {
            return '';
        }
        try {
            $data = $view->getData();
            $page = is_array($data) ? ($data['page'] ?? null) : null;
            $component = is_array($page) ? ($page['component'] ?? null) : null;

            return is_string($component) ? trim($component) : '';
        } catch (Throwable) {
            return '';
        }
    }

    /**
     * The class the check was made against: the first argument, when it is an
     * object or a class name. Cheap, and not the object's contents.
     *
     * @param array<mixed> $arguments
     */
    private static function gateTarget(array $arguments): string
    {
        $first = $arguments[0] ?? null;
        if (is_object($first)) {
            return $first::class;
        }
        if (is_string($first) && $first !== '' && (class_exists($first) || interface_exists($first))) {
            return $first;
        }

        return '';
    }

    /**
     * A type/class list of the check's arguments, never the argument values.
     *
     * Objects become their class, scalars stay clipped, arrays stay a count.
     * That is the Debugbar row without dumping a User into the spool.
     *
     * @param array<mixed> $arguments
     */
    private static function gateArgumentSummary(array $arguments): string
    {
        $parts = [];
        foreach (array_slice($arguments, 0, 8) as $argument) {
            $parts[] = match (true) {
                is_object($argument) => $argument::class,
                is_bool($argument) => $argument ? 'true' : 'false',
                is_int($argument), is_float($argument) => (string) $argument,
                is_string($argument) => self::clip($argument, 64),
                is_array($argument) => 'array('.count($argument).')',
                $argument === null => 'null',
                default => get_debug_type($argument),
            };
        }

        return implode(', ', $parts);
    }

    private static function shortClass(string $class): string
    {
        $class = trim($class);
        if ($class === '') {
            return '';
        }
        $slash = strrpos($class, '\\');

        return $slash === false ? $class : substr($class, $slash + 1);
    }

    /**
     * The model class behind an `eloquent.<hook>: App\Models\User` event.
     *
     * The name carries the class, and the payload carries the instance; the name
     * is preferred because it is a string already and reading it cannot touch the
     * model. The payload is the fallback for any dispatcher that delivers a bare
     * hook name.
     *
     * @param array<mixed> $payload
     */
    private static function modelClassFromEvent(string $name, array $payload): string
    {
        $separator = strpos($name, ': ');
        if ($separator !== false) {
            $class = trim(substr($name, $separator + 2));
            if ($class !== '') {
                return $class;
            }
        }
        $model = $payload[0] ?? null;

        return is_object($model) ? $model::class : '';
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

    /**
     * What was sent, named the way the application names it.
     *
     * A notification says so in the message data. A plain Mailable does not: by
     * the time `MessageSent` fires, Laravel has reduced the Mailable to a view
     * name and a data array, and the class is nowhere in the event — it is still
     * on the CALL STACK, though, because `Mailable::send()` is what is running.
     * Reading it back from there is the only way to record `App\\Mail\\OrderShipped`
     * instead of the literal string `mail`, and it is bounded work: one
     * argument-free backtrace, walked until the first Mailable frame.
     *
     * The subject line would have been the easy answer and is deliberately not
     * used — subjects carry order numbers and customer names, and this class does
     * not record payloads.
     */
    private static function mailType(object $event): string
    {
        $data = is_array($event->data ?? null) ? $event->data : [];
        foreach (['__laravel_notification', '__laravel_notification_class'] as $key) {
            if (isset($data[$key]) && is_string($data[$key]) && $data[$key] !== '') {
                return $data[$key];
            }
        }

        return self::mailableFromCallStack() ?? 'mail';
    }

    /** The first `Illuminate\Mail\Mailable` subclass on the stack, if one is sending. */
    private static function mailableFromCallStack(): ?string
    {
        try {
            if (!class_exists(\Illuminate\Mail\Mailable::class)) {
                return null;
            }
            foreach (debug_backtrace(DEBUG_BACKTRACE_IGNORE_ARGS, 40) as $frame) {
                $class = $frame['class'] ?? null;
                if (is_string($class) && $class !== '' && is_subclass_of($class, \Illuminate\Mail\Mailable::class)) {
                    return $class;
                }
            }
        } catch (Throwable) {
        }

        return null;
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
