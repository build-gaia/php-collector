<?php

declare(strict_types=1);

namespace Chronos\Collector\Dto;

/**
 * A parsed Deterministic-Simulation-Testing (DST) directive carried on an inbound request via the
 * 'X-Chronos-DST' header or the 'chronos_dst' cookie. The directive opts a single request into DST
 * recording out of band from the ambient CHRONOS_PHP_DST_RECORD configuration, so an operator can
 * ask a specific replay-candidate request to be captured without turning recording on estate-wide.
 *
 * Grammar (case-insensitive, whitespace-tolerant), all forms non-throwing:
 *   off                     -> record = false
 *   record                  -> record = true, sampled = null
 *   record:sampled=1.0      -> record = true, sampled = 1.0 (0.0 .. 1.0, clamped)
 *
 * Anything unrecognised parses to null (absent), never an exception: a malformed directive must
 * never disturb the request, it simply means "no DST directive present".
 */
final class DstDirective
{
    public function __construct(
        public readonly bool $record,
        public readonly ?float $sampled,
        public readonly string $raw,
    ) {
    }

    /**
     * Parses a raw directive string (header value or cookie value). Returns null when the value is
     * absent, empty, or unrecognised. Never throws.
     */
    public static function parse(?string $value): ?self
    {
        if (!is_string($value)) {
            return null;
        }
        $raw = trim($value);
        if ($raw === '') {
            return null;
        }
        $lower = strtolower($raw);
        $directive = trim(explode(':', $lower, 2)[0]);
        if ($directive === 'off') {
            return new self(false, null, $raw);
        }
        if ($directive !== 'record') {
            return null;
        }
        $sampled = null;
        if (str_contains($lower, ':')) {
            $modifiers = substr($lower, strpos($lower, ':') + 1);
            foreach (explode(';', $modifiers) as $modifier) {
                $modifier = trim($modifier);
                if (str_starts_with($modifier, 'sampled=')) {
                    $candidate = substr($modifier, strlen('sampled='));
                    if (is_numeric($candidate)) {
                        $sampled = max(0.0, min(1.0, (float) $candidate));
                    }
                }
            }
        }

        return new self(true, $sampled, $raw);
    }
}
