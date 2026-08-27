<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * A bounded catalog of normalised activity records, encoded as one JSON array
 * span attribute.
 *
 * This is the shape ADR 0024 §2/§3 replaced the `laravel.events` /
 * `laravel.jobs` count dictionaries with, and the reason is that a count answers
 * only "did it happen". An event has a destination, a transport, an encoding and
 * a call site; a job has a queue, a transport and the file it will run from.
 * Those are the questions asked immediately after "did it happen", and none of
 * them fits in `{"name": 3}`.
 *
 * The count is not lost. Records are DEDUPED on an identity the caller declares,
 * and repeats increment `count` on the entry already held — so a request that
 * dispatches the same event forty times is one entry saying forty, not forty
 * entries, and not a cap hit that hides the other thirty-nine.
 *
 * Every bound is explicit and a hit bound is reported rather than hidden:
 * `truncated()` is what the companion `.truncated` attribute is written from. The
 * native extension caps an attribute value at 8 KiB and would cut mid-JSON, so
 * the encoded form is bounded here, where the cut can be an honest dropped entry
 * instead of a corrupt string.
 */
final class ActivityCatalog
{
    /** Distinct records held. Past this, repeats still count but new names drop. */
    private const MAX_ENTRIES = 16;

    /** Per-field character bound. A class name is long; a file path is longer. */
    private const MAX_FIELD = 256;

    /**
     * Encoded bound, comfortably inside the native extension's 8 KiB attribute
     * cap so the JSON is never cut mid-structure.
     */
    private const MAX_JSON_BYTES = 6144;

    /** @var array<string, array<string, string|int>> identity → record */
    private array $entries = [];

    private int $dropped = 0;

    /**
     * Record one occurrence.
     *
     * `$fields` is a CALLABLE, not an array, and that is load-bearing rather than
     * stylistic. Resolving a record costs a `debug_backtrace` for the call site
     * and a reflection lookup for the handler file; a request that dispatches the
     * same event four hundred times must pay that once, not four hundred times.
     * A repeat and a cap-hit both return before the callable is ever invoked.
     *
     * @param string                                       $identity what makes two occurrences "the same thing"
     * @param callable(): array<string, string|int|null>    $fields   null and empty values are omitted, never written blank
     */
    public function record(string $identity, callable $fields): void
    {
        if ($identity === '') {
            return;
        }
        if (isset($this->entries[$identity])) {
            $existing = $this->entries[$identity];
            $count = $existing['count'] ?? 1;
            $this->entries[$identity]['count'] = (is_int($count) ? $count : 1) + 1;

            return;
        }
        if (count($this->entries) >= self::MAX_ENTRIES) {
            ++$this->dropped;

            return;
        }

        $record = [];
        foreach ($fields() as $key => $value) {
            if ($value === null || $value === '') {
                continue;
            }
            $record[$key] = is_int($value) ? $value : self::clip((string) $value);
        }
        if ($record === []) {
            return;
        }
        $record['count'] = 1;
        $this->entries[$identity] = $record;
    }

    public function isEmpty(): bool
    {
        return $this->entries === [];
    }

    /**
     * Whether anything was left out — either a distinct record past the entry
     * cap, or a record dropped to keep the encoded form inside its byte bound.
     */
    public function truncated(): bool
    {
        return $this->dropped > 0;
    }

    /**
     * The catalog as a JSON array, busiest first.
     *
     * Busiest first because the bound bites at the tail: if something has to be
     * dropped, the thing that happened once is the better loss than the thing
     * that happened four hundred times.
     */
    public function toJson(): string
    {
        if ($this->entries === []) {
            return '';
        }
        $records = array_values($this->entries);
        usort($records, static function (array $left, array $right): int {
            $leftCount = is_int($left['count'] ?? null) ? $left['count'] : 0;
            $rightCount = is_int($right['count'] ?? null) ? $right['count'] : 0;
            if ($leftCount !== $rightCount) {
                return $rightCount <=> $leftCount;
            }

            return strcmp((string) ($left['name'] ?? ''), (string) ($right['name'] ?? ''));
        });

        // Shrink from the tail until it fits, counting each loss. Encoding and
        // re-encoding is cheap at these sizes and it is the only way to know the
        // real byte length of a JSON array of variable-length strings.
        while ($records !== []) {
            $encoded = json_encode($records, JSON_UNESCAPED_SLASHES);
            if (!is_string($encoded)) {
                return '';
            }
            if (strlen($encoded) <= self::MAX_JSON_BYTES) {
                return $encoded;
            }
            array_pop($records);
            ++$this->dropped;
        }

        return '';
    }

    /**
     * Write the catalog and its truncation companion into an attribute bag.
     *
     * @param array<string, string> $attributes
     */
    public function putInto(array &$attributes, string $key): void
    {
        $encoded = $this->toJson();
        if ($encoded === '') {
            return;
        }
        $attributes[$key] = $encoded;
        if ($this->truncated()) {
            $attributes[$key.'.truncated'] = 'true';
        }
    }

    public function reset(): void
    {
        $this->entries = [];
        $this->dropped = 0;
    }

    private static function clip(string $value): string
    {
        $trimmed = trim($value);
        if ($trimmed === '' || mb_strlen($trimmed) <= self::MAX_FIELD) {
            return $trimmed;
        }

        return mb_substr($trimmed, 0, self::MAX_FIELD);
    }
}
