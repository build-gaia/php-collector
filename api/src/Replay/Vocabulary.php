<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * The recorded vocabulary (protocol §6.1–§6.3, §7.4): which channel a recorded `kind`
 * belongs to, which kinds pair as request and answer, how a selector is derived from a
 * payload, how intent is inferred, and what a `simulated` mode is allowed to invent.
 *
 * Every table here is part of the CONTRACT, not of the PHP implementation. A Node or Go
 * runtime replaying the same recording has to reproduce this file's answers exactly or the
 * two runtimes will select different events from the same recording — so nothing
 * PHP-specific is allowed in, and the one PHP-specific thing that exists (a call site's
 * `file:line`) is deliberately excluded from selection and admitted only as diagnostics.
 */
final class Vocabulary
{
    public const CHANNEL_TIME = 'time';
    public const CHANNEL_RANDOM = 'random';
    public const CHANNEL_DATABASE = 'database';
    public const CHANNEL_CACHE = 'cache';
    public const CHANNEL_HTTP = 'http';
    public const CHANNEL_FILE = 'file';
    public const CHANNEL_ENV = 'env';
    public const CHANNEL_QUEUE = 'queue';

    public const CUSTOM_PREFIX = 'custom:';

    public const INTENT_READ = 'read';
    public const INTENT_WRITE = 'write';

    /** A recorded event that asks something and expects a paired answer event. */
    public const ROLE_REQUEST = 'request';

    /** A recorded event that answers an earlier request event. */
    public const ROLE_ANSWER = 'answer';

    /** A recorded event that carries its own answer. Most kinds are this. */
    public const ROLE_SELF = 'self';

    /**
     * The only request/answer pairs v1 defines. Everything else answers itself; adding a
     * pair is a MINOR protocol change, changing one is a MAJOR change.
     */
    private const PAIRS = [
        'database_query' => 'database_result',
        'http_request' => 'http_response',
    ];

    /**
     * Channel for a recorded kind. An unrecognised kind is NEVER dropped — it becomes
     * `custom:<kind>` and is served like any other channel.
     *
     * That last rule is what makes minor-version forward compatibility real: a runtime that
     * discarded kinds it did not know would turn a future recorder's richer capture into a
     * silent under-replay, and an under-replay is indistinguishable from conformance in the
     * report. A kind already spelled `custom:<name>` keeps that spelling rather than being
     * prefixed twice.
     */
    public static function channelForKind(string $kind): string
    {
        return match ($kind) {
            'time' => self::CHANNEL_TIME,
            'random' => self::CHANNEL_RANDOM,
            'database_query', 'database_result' => self::CHANNEL_DATABASE,
            'cache_read', 'cache_write' => self::CHANNEL_CACHE,
            'http_request', 'http_response' => self::CHANNEL_HTTP,
            'file_read', 'file_write' => self::CHANNEL_FILE,
            'env_read' => self::CHANNEL_ENV,
            default => match (true) {
                str_starts_with($kind, 'queue_') => self::CHANNEL_QUEUE,
                str_starts_with($kind, self::CUSTOM_PREFIX) => $kind,
                default => self::CUSTOM_PREFIX.$kind,
            },
        };
    }

    public static function roleForKind(string $kind): string
    {
        if (isset(self::PAIRS[$kind])) {
            return self::ROLE_REQUEST;
        }

        return in_array($kind, self::PAIRS, true) ? self::ROLE_ANSWER : self::ROLE_SELF;
    }

    /** The answer kind a request kind binds to, or null when the kind is not a request. */
    public static function answerKindFor(string $kind): ?string
    {
        return self::PAIRS[$kind] ?? null;
    }

    /**
     * Derive a selector from a recorded payload (protocol §6.2), first match wins:
     * the explicit `chronos.selector` escape hatch, then the channel's conventional keys,
     * then the recorded symbol under `function`, then the empty string.
     *
     * The empty string is a valid selector and not a failure — it is what `random` events
     * carrying only a result normalise to, and two such events are still distinguishable by
     * their ordinal within the channel.
     *
     * @param array<string, mixed> $payload
     */
    public static function selectorFor(string $channel, array $payload): string
    {
        $explicit = self::value($payload, 'chronos.selector');
        if ($explicit !== null) {
            return Canonical::selector($explicit);
        }
        $conventional = self::conventional($channel, $payload);
        if ($conventional !== null) {
            return Canonical::selector($conventional);
        }
        $symbol = self::value($payload, 'function');

        return $symbol === null ? '' : Canonical::selector($symbol);
    }

    /**
     * Intent for a recorded kind (protocol §6.3). Read for anything named `*_read`, write for
     * `*_write` and every queue kind, and for HTTP by method — a method that is not GET, HEAD
     * or OPTIONS changes something at the other end.
     *
     * @param array<string, mixed> $payload
     */
    public static function intentForKind(string $kind, array $payload = []): string
    {
        if ($kind === 'database_query' || $kind === 'database_result') {
            return self::intentForStatement(
                Canonical::selector(self::value($payload, 'statement')
                    ?? self::value($payload, 'query')
                    ?? self::value($payload, 'sql')
                    ?? ''),
            );
        }
        if ($kind === 'http_request' || $kind === 'http_response') {
            $method = strtoupper(Canonical::selector(self::value($payload, 'method') ?? 'GET'));

            return in_array($method, ['GET', 'HEAD', 'OPTIONS'], true)
                ? self::INTENT_READ
                : self::INTENT_WRITE;
        }
        if (str_ends_with($kind, '_read')) {
            return self::INTENT_READ;
        }

        return str_ends_with($kind, '_write') || str_starts_with($kind, 'queue_')
            ? self::INTENT_WRITE
            : self::INTENT_READ;
    }

    /**
     * The protocol's default SQL intent heuristic: the first keyword of the normalised
     * statement decides. A runtime MAY do better and MUST document it if it does; this
     * implementation does not, because a PHP driver hands us a statement string and nothing
     * else, and guessing harder would only move the mistakes around.
     */
    public static function intentForStatement(string $statement): string
    {
        $normalised = Canonical::selector($statement);
        // Split on space or the opening parenthesis a bare `(SELECT …)` union starts with.
        // strtok() would be shorter and leaves process-global state behind; a replay whose
        // answers depended on that would not be the deterministic thing it advertises.
        preg_match('/^[^ (]*/', $normalised, $matches);
        $keyword = strtoupper($matches[0] ?? '');

        return in_array($keyword, ['SELECT', 'SHOW', 'DESCRIBE', 'EXPLAIN', 'WITH'], true)
            ? self::INTENT_READ
            : self::INTENT_WRITE;
    }

    /**
     * The declared synthetic answer for a `simulated` miss (protocol §7.4), or null for the
     * two channels simulation is forbidden on.
     *
     * `time` and `random` return null because a synthetic clock reading or RNG value is
     * indistinguishable from a real one: inventing them would silently de-determinise the
     * replay, which is the single outcome this protocol exists to prevent. HTTP 599 is
     * outside the registered status range so that application code which never reads the
     * report can still tell the response was synthetic.
     *
     * @return array<string, string>|null
     */
    public static function syntheticAnswer(string $channel, string $intent, int $ordinal): ?array
    {
        if ($channel === self::CHANNEL_TIME || $channel === self::CHANNEL_RANDOM) {
            return null;
        }
        $write = $intent === self::INTENT_WRITE;

        return match (true) {
            $channel === self::CHANNEL_DATABASE && $write => ['rows' => '0', 'simulated' => '1'],
            $channel === self::CHANNEL_DATABASE => ['rows' => '0', 'body' => '[]', 'simulated' => '1'],
            $channel === self::CHANNEL_HTTP => [
                'status' => '599',
                'body' => '',
                'headers' => 'X-Chronos-Simulated: 1',
            ],
            $channel === self::CHANNEL_CACHE => ['result' => 'miss', 'value' => '', 'simulated' => '1'],
            self::isQueueChannel($channel) => [
                'accepted' => 'true',
                'messageId' => 'chronos-simulated-'.$ordinal,
                'simulated' => '1',
            ],
            $channel === self::CHANNEL_FILE && $write => ['bytes' => '0', 'simulated' => '1'],
            $channel === self::CHANNEL_FILE => ['body' => '', 'simulated' => '1'],
            $channel === self::CHANNEL_ENV => ['value' => '', 'simulated' => '1'],
            default => ['simulated' => '1'],
        };
    }

    /**
     * Whether a channel is a queue channel for policy purposes. Both spellings are
     * recognised because the recorded vocabulary reserves no queue kind: the PHP native
     * collector emits queue activity as a custom kind, and the scheduler's own planner
     * already matches `queue_*` and `custom:queue*` the same way.
     */
    public static function isQueueChannel(string $channel): bool
    {
        return $channel === self::CHANNEL_QUEUE
            || str_starts_with($channel, self::CUSTOM_PREFIX.'queue');
    }

    /**
     * The environment-variable token for a channel: upper-cased with every character outside
     * [A-Za-z0-9] replaced by `_`, so `custom:widget` governs through
     * CHRONOS_REPLAY_EFFECT_CUSTOM_WIDGET_READ. The names this produces satisfy the Docker
     * executor's environment-name rule, so no plan can be rejected for using one.
     */
    public static function environmentToken(string $channel): string
    {
        return (string) preg_replace('/[^A-Za-z0-9]/', '_', strtoupper($channel));
    }

    /** @param array<string, mixed> $payload */
    private static function conventional(string $channel, array $payload): ?string
    {
        if ($channel === self::CHANNEL_DATABASE) {
            return self::value($payload, 'statement')
                ?? self::value($payload, 'query')
                ?? self::value($payload, 'sql');
        }
        if ($channel === self::CHANNEL_HTTP) {
            $method = self::value($payload, 'method');
            $url = self::value($payload, 'url');
            if ($method !== null && $url !== null) {
                return strtoupper(Canonical::selector($method)).' '.$url;
            }

            return $url ?? self::value($payload, 'request');
        }
        if ($channel === self::CHANNEL_CACHE) {
            return self::value($payload, 'key') ?? self::value($payload, 'cacheKey');
        }
        if ($channel === self::CHANNEL_FILE) {
            return self::value($payload, 'path') ?? self::value($payload, 'file');
        }
        if ($channel === self::CHANNEL_ENV) {
            return self::value($payload, 'name') ?? self::value($payload, 'variable');
        }
        if ($channel === self::CHANNEL_TIME || $channel === self::CHANNEL_RANDOM) {
            return null;
        }

        // queue and every custom channel.
        return self::value($payload, 'key')
            ?? self::value($payload, 'name')
            ?? self::value($payload, 'topic');
    }

    /** @param array<string, mixed> $payload */
    private static function value(array $payload, string $key): ?string
    {
        return array_key_exists($key, $payload) ? Canonical::text($payload[$key]) : null;
    }
}
