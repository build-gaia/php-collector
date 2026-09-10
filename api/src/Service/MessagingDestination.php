<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Throwable;

/**
 * WHERE a message went, in one vocabulary every broker can be described in.
 *
 * ADR 0024 §2 gave messaging a normalised shape and stopped at the leaf name:
 * `messaging.destination.name` is the queue, the topic, the subject. On one
 * broker that is an address; on an estate it is not. A RabbitMQ queue called
 * `products` exists in six vhosts of this organisation's own cluster, so a span
 * that names only `products` cannot be joined to a stream — the desktop's
 * producer graph either attaches the trace to all six or to none, and both are
 * wrong in a way that looks like data.
 *
 * So a destination is described by four normalised facts, and every broker fills
 * in the ones it has:
 *
 * | Key | Means | RabbitMQ | Kafka | NATS | SQS | Redis / database |
 * | --- | --- | --- | --- | --- | --- | --- |
 * | `…destination.name` | The leaf a consumer reads | queue | topic | subject | queue | list key / table |
 * | `…destination.namespace` | What scopes that name | vhost | cluster | account | region | connection |
 * | `…destination.via` | What ROUTED it there | exchange | – | JetStream stream | SNS topic | – |
 * | `…destination.route` | The selector used against `via` | routing key | – | subject filter | – | – |
 *
 * The point of the table is that the KEYS do not change. A reader — and the
 * graph — learns "namespace scopes the name" once, rather than learning vhost
 * for one broker and account for the next; and a broker with no exchange simply
 * omits `via` rather than filling it with a synonym for the name. Vendor words
 * survive where they are the only honest ones: `messaging.system` still says
 * `rabbitmq`, which is what tells a reader that `namespace` is a vhost.
 *
 * ## Absent, never guessed
 *
 * Every field is omitted when it cannot be read. A namespace inferred from a
 * default ("probably `/`") would be worse than no namespace at all: the join
 * would become confident and wrong, where an absent namespace degrades to the
 * name-only match the desktop already labels as ambiguous.
 *
 * ## Read from CONFIG, never from the broker
 *
 * Hydration is a lookup in the application's own already-loaded configuration —
 * the same `config()` read `QueueTelemetry::queueDriver()` has always done. No
 * connection is opened, no application code runs, and nothing here can fail in a
 * way the application notices: every path is wrapped, and a throw yields fewer
 * attributes rather than a broken publish.
 */
final class MessagingDestination
{
    public const NAME = 'messaging.destination.name';
    public const NAMESPACE_KEY = 'messaging.destination.namespace';
    public const VIA = 'messaging.destination.via';
    public const ROUTE = 'messaging.destination.route';

    /**
     * The normalised destination attributes for one Laravel queue connection.
     *
     * @param string $system     the resolved driver: rabbitmq, redis, sqs, database, kafka…
     * @param string $connection the connection NAME, as `queue.connections.<name>`
     * @param string $queue      the leaf destination, when the caller knows it
     *
     * @return array<string, string> only the facts that could be read
     */
    public static function forLaravelQueue(
        string $system,
        string $connection,
        string $queue = '',
    ): array {
        $attributes = [];
        if ($queue !== '') {
            $attributes[self::NAME] = $queue;
        }
        try {
            $config = self::connectionConfig($connection);
            if ($config === []) {
                return $attributes;
            }

            return $attributes + self::fromConfig($system, $config);
        } catch (Throwable) {
            return $attributes;
        }
    }

    /**
     * The same mapping, over a configuration array that is already in hand.
     *
     * Split out from the `config()` lookup so the vocabulary is testable without
     * a framework: this is the half that decides what a vhost IS in the
     * normalised vocabulary, and it is the half worth pinning down.
     *
     * @param array<string, mixed> $config one entry of `queue.connections`
     *
     * @return array<string, string>
     */
    public static function fromConfig(string $system, array $config): array
    {
        return match ($system) {
            'rabbitmq', 'amqp' => self::amqp($config),
            'sqs' => self::filled([
                // The region is what makes an SQS queue name unique, and the
                // account id lives in the prefix URL rather than as a field.
                self::NAMESPACE_KEY => self::text($config, 'region'),
            ]),
            'redis' => self::filled([
                // Which Redis, not which key: two apps sharing a cluster with
                // different connections are different namespaces.
                self::NAMESPACE_KEY => self::text($config, 'connection'),
            ]),
            'database' => self::filled([
                self::NAMESPACE_KEY => self::text($config, 'connection'),
            ]),
            'kafka' => self::filled([
                // Only when the application NAMES a cluster. A broker list is a
                // connection detail, not an identity, and the first host in it
                // is not the cluster's name.
                self::NAMESPACE_KEY => self::text($config, 'cluster'),
            ]),
            default => [],
        };
    }

    /**
     * AMQP: the vhost scopes the queue, the exchange routed it, the routing key
     * is the selector that chose it.
     *
     * The vhost is read from the multi-host form first (`hosts.0.vhost`, which
     * is what `vladimir-yuldashev/laravel-queue-rabbitmq` writes) and from the
     * flat form second, because an application configured either way is
     * describing the same fact.
     *
     * @param array<string, mixed> $config
     *
     * @return array<string, string>
     */
    private static function amqp(array $config): array
    {
        $hosts = $config['hosts'] ?? null;
        $first = is_array($hosts) ? ($hosts[0] ?? null) : null;
        $vhost = is_array($first) ? self::text($first, 'vhost') : '';
        if ($vhost === '') {
            $vhost = self::text($config, 'vhost');
        }
        $options = is_array($config['options'] ?? null) ? $config['options'] : [];
        $exchange = is_array($options['exchange'] ?? null) ? $options['exchange'] : [];
        $queue = is_array($options['queue'] ?? null) ? $options['queue'] : [];

        return self::filled([
            self::NAMESPACE_KEY => $vhost,
            self::VIA => self::text($exchange, 'name'),
            self::ROUTE => self::text($queue, 'routing_key'),
        ]);
    }

    /**
     * The connection's configuration, or an empty array where there is no
     * framework to ask.
     *
     * @return array<string, mixed>
     */
    private static function connectionConfig(string $connection): array
    {
        if ($connection === '' || !function_exists('config')) {
            return [];
        }
        $config = config("queue.connections.{$connection}");

        return is_array($config) ? $config : [];
    }

    /**
     * @param array<string|int, mixed> $config
     */
    private static function text(array $config, string $key): string
    {
        $value = $config[$key] ?? null;

        return is_scalar($value) ? trim((string) $value) : '';
    }

    /**
     * Drop the empty ones — see the note on absent-never-guessed.
     *
     * @param array<string, string> $attributes
     *
     * @return array<string, string>
     */
    private static function filled(array $attributes): array
    {
        return array_filter($attributes, static fn (string $value): bool => $value !== '');
    }
}
