<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * ADR 0021 Phase 2/3 — ordered first-party call visits from a DST recording, and the first
 * divergent frame between two paths.
 *
 * Call events are observational (`kind=call`, payload `name` + `depth`). They are not Effect
 * lookups. Phase 3 uses {@see self::firstDivergence()} after a mutated replay records a new path.
 */
final class CallPath
{
    /**
     * Visits noted during this process (Phase 3 executed path). Cleared on
     * {@see ReplayRuntime::reset()} and when a new session boots.
     *
     * @var list<array{name: string, depth: int}>
     */
    private static array $executed = [];

    /**
     * Note one executed frame. Used by tests and by a future replay-time observer hook;
     * recordings already carry `kind=call` events for the baseline path.
     */
    public static function note(string $name, int $depth = 0): void
    {
        if ($name === '') {
            return;
        }
        self::$executed[] = ['name' => $name, 'depth' => $depth];
    }

    /**
     * @return list<array{name: string, depth: int}>
     */
    public static function executed(): array
    {
        return self::$executed;
    }

    public static function resetExecuted(): void
    {
        self::$executed = [];
    }

    /**
     * Load a path from a JSON file: either a bare list of `{name,depth}` frames, an
     * `{events:[…]}` recording fragment, or a top-level events array.
     *
     * @return list<array{name: string, depth: int}>
     */
    public static function fromFile(string $path): array
    {
        if ($path === '' || !is_file($path)) {
            return [];
        }
        $raw = json_decode((string) file_get_contents($path), true);
        if (!is_array($raw)) {
            return [];
        }
        if (isset($raw['events']) && is_array($raw['events'])) {
            return self::fromEvents($raw['events']);
        }
        if (array_is_list($raw)) {
            return self::fromEvents($raw);
        }

        return [];
    }

    /**
     * Report fragment for Phase 3 (protocol §9 allows unknown fields). Null when there is
     * nothing useful to attach (no recorded path and no executed path).
     *
     * @param list<array{name: string, depth: int}> $recorded
     * @param list<array{name: string, depth: int}> $executed
     * @return array{
     *     recordedCount: int,
     *     executedCount: int,
     *     firstDivergence: array{index: int, recorded: ?array{name: string, depth: int}, executed: ?array{name: string, depth: int}}|null
     * }|null
     */
    public static function reportFragment(array $recorded, array $executed): ?array
    {
        if ($recorded === [] && $executed === []) {
            return null;
        }

        return [
            'recordedCount' => count($recorded),
            'executedCount' => count($executed),
            'firstDivergence' => self::firstDivergence($recorded, $executed),
        ];
    }

    /**
     * @param list<RecordedEvent>|list<array{name?: mixed, depth?: mixed}|mixed> $events
     * @return list<array{name: string, depth: int}>
     */
    public static function fromEvents(array $events): array
    {
        $path = [];
        foreach ($events as $event) {
            if ($event instanceof RecordedEvent) {
                if ($event->kind !== 'call') {
                    continue;
                }
                $payload = $event->payload;
            } elseif (is_array($event)) {
                $kind = $event['kind'] ?? null;
                $payload = $event['payload'] ?? $event;
                if ($kind !== null && $kind !== 'call') {
                    continue;
                }
                if (!is_array($payload)) {
                    continue;
                }
            } else {
                continue;
            }
            $name = $payload['name'] ?? null;
            if (!is_string($name) || $name === '') {
                continue;
            }
            $depth = $payload['depth'] ?? 0;
            $path[] = [
                'name' => $name,
                'depth' => is_numeric($depth) ? (int) $depth : 0,
            ];
        }

        return $path;
    }

    /**
     * @param list<array{name: string, depth: int}> $recorded
     * @param list<array{name: string, depth: int}> $executed
     * @return array{index: int, recorded: ?array{name: string, depth: int}, executed: ?array{name: string, depth: int}}|null
     */
    public static function firstDivergence(array $recorded, array $executed): ?array
    {
        $limit = max(count($recorded), count($executed));
        for ($index = 0; $index < $limit; ++$index) {
            $left = $recorded[$index] ?? null;
            $right = $executed[$index] ?? null;
            if ($left === $right) {
                continue;
            }
            if (
                is_array($left)
                && is_array($right)
                && ($left['name'] ?? null) === ($right['name'] ?? null)
                && (int) ($left['depth'] ?? 0) === (int) ($right['depth'] ?? 0)
            ) {
                continue;
            }

            return [
                'index' => $index,
                'recorded' => is_array($left) ? $left : null,
                'executed' => is_array($right) ? $right : null,
            ];
        }

        return null;
    }
}
