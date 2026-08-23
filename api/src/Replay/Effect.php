<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * The call surface the replayed application reaches: one method per effect class, each
 * returning the recorded answer or null.
 *
 * `null` means "this process is not a replay" — perform the real effect. Every other outcome is
 * handled here rather than by the caller: a blocked effect throws, an unanswerable one ends the
 * process. That asymmetry is the point. An interception layer written against this class cannot
 * accidentally implement the failure mode the protocol forbids, because the only way to get a
 * value back is for the recording to have had one.
 *
 * This is also where PHP's own vocabulary stops. Above here a PDO wrapper, a Guzzle middleware
 * or a native hook decides what a "statement" or a "url" is; below here nothing knows what
 * language it is running in.
 */
final class Effect
{
    /** @param array<string, mixed>|null $expectation */
    public static function database(
        string $statement,
        ?string $intent = null,
        ?array $expectation = null,
        ?callable $live = null,
        ?string $site = null,
    ): ?array {
        return self::resolve(new Lookup(
            Vocabulary::CHANNEL_DATABASE,
            $statement,
            $intent ?? Vocabulary::intentForStatement($statement),
            $expectation,
            $site,
        ), $live);
    }

    /** @param array<string, mixed>|null $expectation */
    public static function http(
        string $method,
        string $url,
        ?array $expectation = null,
        ?callable $live = null,
        ?string $site = null,
    ): ?array {
        return self::resolve(new Lookup(
            Vocabulary::CHANNEL_HTTP,
            strtoupper(Canonical::selector($method)).' '.$url,
            Vocabulary::intentForKind('http_request', ['method' => $method]),
            $expectation,
            $site,
        ), $live);
    }

    /** @param array<string, mixed>|null $expectation */
    public static function cache(
        string $key,
        string $intent = Vocabulary::INTENT_READ,
        ?array $expectation = null,
        ?callable $live = null,
        ?string $site = null,
    ): ?array {
        return self::resolve(new Lookup(Vocabulary::CHANNEL_CACHE, $key, $intent, $expectation, $site), $live);
    }

    /** @param array<string, mixed>|null $expectation */
    public static function queue(
        string $name,
        ?array $expectation = null,
        ?callable $live = null,
        ?string $site = null,
    ): ?array {
        // Publishing is a write by definition; there is no read side of a queue publish, and
        // treating one as a read would route it past CHRONOS_REPLAY_EFFECT_QUEUE_PUBLISH.
        return self::resolve(new Lookup(
            Vocabulary::CHANNEL_QUEUE,
            $name,
            Vocabulary::INTENT_WRITE,
            $expectation,
            $site,
        ), $live);
    }

    /** @param array<string, mixed>|null $expectation */
    public static function file(
        string $path,
        string $intent = Vocabulary::INTENT_READ,
        ?array $expectation = null,
        ?callable $live = null,
        ?string $site = null,
    ): ?array {
        return self::resolve(new Lookup(Vocabulary::CHANNEL_FILE, $path, $intent, $expectation, $site), $live);
    }

    /**
     * An environment read. Named `environment` rather than `env` because the recorded channel is
     * `env` and a method called `env()` in a Laravel application already means something else.
     */
    public static function environment(string $name, ?string $site = null): ?array
    {
        return self::resolve(new Lookup(Vocabulary::CHANNEL_ENV, $name, Vocabulary::INTENT_READ, null, $site));
    }

    /**
     * A clock read, keyed on the PHP symbol that made it (`time`, `microtime`, `hrtime`).
     *
     * The protocol is honest that this is its weak spot: a clock read carries no inputs, so the
     * channel degrades to ordinal-within-symbol and is order-sensitive. Because the cursor is
     * per (channel, selector), an extra clock read perturbs clock answers only — it cannot reach
     * the database answers — and the divergence it produces names the channel explicitly.
     */
    public static function time(string $function, ?string $site = null): ?array
    {
        return self::resolve(new Lookup(Vocabulary::CHANNEL_TIME, $function, Vocabulary::INTENT_READ, null, $site));
    }

    /** A randomness read, keyed on the PHP symbol that made it (`mt_rand`, `random_int`). */
    public static function random(string $function, ?string $site = null): ?array
    {
        return self::resolve(new Lookup(Vocabulary::CHANNEL_RANDOM, $function, Vocabulary::INTENT_READ, null, $site));
    }

    /**
     * Any effect class this runtime records but the protocol does not name. The channel becomes
     * `custom:<kind>` and gets its own policy variable with no protocol change.
     *
     * @param array<string, mixed>|null $expectation
     */
    public static function custom(
        string $kind,
        string $selector,
        string $intent = Vocabulary::INTENT_READ,
        ?array $expectation = null,
        ?callable $live = null,
        ?string $site = null,
    ): ?array {
        return self::resolve(new Lookup(
            Vocabulary::channelForKind($kind),
            $selector,
            $intent,
            $expectation,
            $site,
        ), $live);
    }

    /**
     * Resolve a lookup against the active session and turn its outcome into PHP.
     *
     * @return array<string, mixed>|null
     *
     * @throws ReplayBlocked when policy refused the effect
     */
    public static function resolve(Lookup $lookup, ?callable $live = null): ?array
    {
        $session = ReplayRuntime::session();
        if ($session === null || !$session->isRunning()) {
            return null;
        }
        $answer = $session->resolve($lookup, $live);
        if ($answer->fatal) {
            // Does not return in a real replay: the report is written and the process exits 92.
            ReplayRuntime::abort($answer);
        }
        if ($answer->outcome === Answer::BLOCKED) {
            throw new ReplayBlocked($answer);
        }

        return $answer->payload;
    }
}
