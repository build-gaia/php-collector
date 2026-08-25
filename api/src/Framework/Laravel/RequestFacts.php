<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Service\NativeExtension;
use Throwable;

/**
 * Bounded Laravel facts stamped onto the request root span at flush.
 *
 * This is the Debugbar-shaped hydration that belongs on a trace: which action
 * ran, who was authenticated, which views/models/mail/jobs/gates/events
 * participated — names and counts only. View data, cache values, session
 * contents, notification bodies and Gate arguments stay out.
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
    private static array $jobs = [];

    /** @var array<string, int> */
    private static array $gates = [];

    /** @var array<string, int> */
    private static array $events = [];

    private static int $droppedViews = 0;

    private static int $droppedModels = 0;

    private static int $droppedMail = 0;

    private static int $droppedJobs = 0;

    private static int $droppedGates = 0;

    private static int $droppedEvents = 0;

    public static function reset(): void
    {
        self::$views = [];
        self::$models = [];
        self::$mail = [];
        self::$jobs = [];
        self::$gates = [];
        self::$events = [];
        self::$droppedViews = 0;
        self::$droppedModels = 0;
        self::$droppedMail = 0;
        self::$droppedJobs = 0;
        self::$droppedGates = 0;
        self::$droppedEvents = 0;
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

    public static function noteJob(string $job, string $queue = ''): void
    {
        $label = $queue === '' ? $job : $job.'@'.$queue;
        self::note(self::$jobs, self::$droppedJobs, $label);
    }

    public static function noteGate(string $ability, bool $allowed): void
    {
        $label = $ability.':'.($allowed ? 'allow' : 'deny');
        self::note(self::$gates, self::$droppedGates, $label);
    }

    public static function noteEvent(string $name): void
    {
        if (self::isFrameworkEvent($name)) {
            return;
        }
        self::note(self::$events, self::$droppedEvents, $name);
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
            $attributes['process.runtime.php.memory.peak_bytes'] = (string) $peakMemoryBytes;
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
        self::putCounts($attributes, 'laravel.views', self::$views, self::$droppedViews);
        self::putCounts($attributes, 'laravel.models', self::$models, self::$droppedModels);
        self::putCounts($attributes, 'laravel.mail', self::$mail, self::$droppedMail);
        self::putCounts($attributes, 'laravel.jobs', self::$jobs, self::$droppedJobs);
        self::putCounts($attributes, 'laravel.gates', self::$gates, self::$droppedGates);
        self::putCounts($attributes, 'laravel.events', self::$events, self::$droppedEvents);

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
                    $queue = isset($observed->queue) && is_string($observed->queue) ? $observed->queue : '';
                    if ($name !== '') {
                        self::noteJob($name, $queue);
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
                $name = $args[0] ?? '';
                if (is_object($name)) {
                    self::noteEvent($name::class);
                    return;
                }
                if (is_string($name)) {
                    self::noteEvent($name);
                }
            });
        } catch (Throwable) {
        }
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
