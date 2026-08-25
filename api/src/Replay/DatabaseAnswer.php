<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * Normalises an {@see Effect::database()} payload into rows a PDO / query wrapper can return.
 *
 * Recordings vary: `rows` may be an array, a JSON string, or a count string with a separate
 * `body`. MutationSweep's `empty_database` profile sets `rows` to `[]` and `rowCount` to `"0"`.
 */
final class DatabaseAnswer
{
    /**
     * @param array<string, mixed> $payload
     * @return list<array<string, mixed>>
     */
    public static function rows(array $payload): array
    {
        $rows = $payload['rows'] ?? null;
        if (is_array($rows)) {
            $out = [];
            foreach ($rows as $row) {
                if (is_array($row)) {
                    $out[] = $row;
                }
            }

            return $out;
        }
        if (is_string($rows) && $rows !== '' && !is_numeric($rows)) {
            $decoded = json_decode($rows, true);
            if (is_array($decoded)) {
                return self::rows(['rows' => $decoded]);
            }
        }
        $body = $payload['body'] ?? null;
        if (is_string($body) && $body !== '') {
            $decoded = json_decode($body, true);
            if (is_array($decoded)) {
                return self::rows(['rows' => $decoded]);
            }
        }

        return [];
    }

    /**
     * @param array<string, mixed> $payload
     */
    public static function rowCount(array $payload): int
    {
        if (isset($payload['rowCount']) && is_numeric($payload['rowCount'])) {
            return (int) $payload['rowCount'];
        }
        if (isset($payload['rows']) && is_numeric($payload['rows'])) {
            return (int) $payload['rows'];
        }

        return count(self::rows($payload));
    }
}
