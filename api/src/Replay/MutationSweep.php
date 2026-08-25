<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * ADR 0021 Phase 4 substrate — bounded mutations of a DST recording for agent sweeps.
 *
 * Produces new event lists without re-hitting live dependencies. Each profile is intentional
 * and small: clock skew, empty database results, HTTP 5xx/empty body. Does not invent
 * call-path events or touch privacy-gated body capture beyond rewriting already-recorded
 * payloads the recording already holds.
 */
final class MutationSweep
{
    public const PROFILE_CLOCK_SKEW = 'clock_skew';
    public const PROFILE_EMPTY_DATABASE = 'empty_database';
    public const PROFILE_HTTP_5XX = 'http_5xx';
    public const PROFILE_HTTP_EMPTY_BODY = 'http_empty_body';

    /**
     * @return list<string>
     */
    public static function profiles(): array
    {
        return [
            self::PROFILE_CLOCK_SKEW,
            self::PROFILE_EMPTY_DATABASE,
            self::PROFILE_HTTP_5XX,
            self::PROFILE_HTTP_EMPTY_BODY,
        ];
    }

    /**
     * @param list<array<string, mixed>> $events
     * @return list<array{profile: string, events: list<array<string, mixed>>}>
     */
    public static function expand(array $events, ?array $profiles = null): array
    {
        $profiles ??= self::profiles();
        $out = [];
        foreach ($profiles as $profile) {
            if (!is_string($profile) || $profile === '') {
                continue;
            }
            $mutated = self::apply($events, $profile);
            if ($mutated === null) {
                continue;
            }
            $out[] = ['profile' => $profile, 'events' => $mutated];
        }

        return $out;
    }

    /**
     * @param list<array<string, mixed>> $events
     * @return list<array<string, mixed>>|null Null when the profile has nothing to mutate.
     */
    public static function apply(array $events, string $profile): ?array
    {
        return match ($profile) {
            self::PROFILE_CLOCK_SKEW => self::mapPayload($events, 'time', static function (array $payload): array {
                if (!isset($payload['result']) || !is_numeric($payload['result'])) {
                    return $payload;
                }
                $payload['result'] = (string) ((int) $payload['result'] + 3600);

                return $payload;
            }),
            self::PROFILE_EMPTY_DATABASE => self::mapPayload($events, 'database_result', static function (array $payload): array {
                $payload['rows'] = [];
                $payload['rowCount'] = '0';
                unset($payload['result'], $payload['body']);

                return $payload;
            }),
            self::PROFILE_HTTP_5XX => self::mapPayload($events, 'http_response', static function (array $payload): array {
                $payload['status'] = '503';
                $payload['body'] = '';

                return $payload;
            }),
            self::PROFILE_HTTP_EMPTY_BODY => self::mapPayload($events, 'http_response', static function (array $payload): array {
                $payload['body'] = '';

                return $payload;
            }),
            default => null,
        };
    }

    /**
     * @param list<array<string, mixed>> $events
     * @param callable(array<string, mixed>): array<string, mixed> $rewrite
     * @return list<array<string, mixed>>|null
     */
    private static function mapPayload(array $events, string $kind, callable $rewrite): ?array
    {
        $changed = false;
        $out = [];
        foreach ($events as $event) {
            if (!is_array($event)) {
                continue;
            }
            if (($event['kind'] ?? null) !== $kind) {
                $out[] = $event;
                continue;
            }
            $payload = $event['payload'] ?? [];
            if (!is_array($payload)) {
                $out[] = $event;
                continue;
            }
            $event['payload'] = $rewrite($payload);
            $changed = true;
            $out[] = $event;
        }

        return $changed ? $out : null;
    }
}
