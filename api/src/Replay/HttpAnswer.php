<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * Normalises an {@see Effect::http()} payload into status / body / headers a client adapter
 * can turn into a response object.
 *
 * Recorded HTTP answers use string fields (`status`, `body`, optional `headers`). Synthetic
 * misses use status 599 (protocol §7.4). Adapters must not invent values when the payload is
 * absent — that is {@see Effect}'s job.
 */
final class HttpAnswer
{
    /**
     * @param array<string, mixed> $payload
     */
    public static function status(array $payload): int
    {
        $status = $payload['status'] ?? 0;
        if (is_numeric($status)) {
            return (int) $status;
        }

        return 0;
    }

    /**
     * @param array<string, mixed> $payload
     */
    public static function body(array $payload): string
    {
        $body = $payload['body'] ?? '';
        if (is_string($body)) {
            return $body;
        }
        if (is_scalar($body)) {
            return (string) $body;
        }

        return '';
    }

    /**
     * @param array<string, mixed> $payload
     * @return array<string, list<string>>
     */
    public static function headers(array $payload): array
    {
        $raw = $payload['headers'] ?? null;
        if (is_array($raw)) {
            $out = [];
            foreach ($raw as $name => $value) {
                if (!is_string($name) || $name === '') {
                    continue;
                }
                if (is_array($value)) {
                    $lines = [];
                    foreach ($value as $entry) {
                        if (is_scalar($entry)) {
                            $lines[] = (string) $entry;
                        }
                    }
                    $out[$name] = $lines;
                    continue;
                }
                if (is_scalar($value)) {
                    $out[$name] = [(string) $value];
                }
            }

            return $out;
        }
        if (!is_string($raw) || $raw === '') {
            return [];
        }
        // Recorder may emit a single header line ("Name: value") or a CRLF block.
        $out = [];
        foreach (preg_split("/\r\n|\n|\r/", $raw) ?: [] as $line) {
            $line = trim($line);
            if ($line === '' || !str_contains($line, ':')) {
                continue;
            }
            [$name, $value] = explode(':', $line, 2);
            $name = trim($name);
            if ($name === '') {
                continue;
            }
            $out[$name][] = trim($value);
        }

        return $out;
    }
}
