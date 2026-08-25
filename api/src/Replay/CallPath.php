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
     * @param list<array{name?: mixed, depth?: mixed}|mixed> $events Recording events or already
     *                                                              extracted call payloads.
     * @return list<array{name: string, depth: int}>
     */
    public static function fromEvents(array $events): array
    {
        $path = [];
        foreach ($events as $event) {
            if (!is_array($event)) {
                continue;
            }
            $kind = $event['kind'] ?? null;
            $payload = $event['payload'] ?? $event;
            if ($kind !== null && $kind !== 'call') {
                continue;
            }
            if (!is_array($payload)) {
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
