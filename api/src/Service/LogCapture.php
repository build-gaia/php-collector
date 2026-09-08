<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * Shared truncation + dispatch seam for every "forward this framework's log record to Chronos"
 * bridge (Symfony 1's ChronosLogListener, Laravel's RichTelemetryHooks, and now the Monolog
 * handler and PSR-3 decorator). Centralised here rather than duplicated per-bridge because the
 * native caps are a NATIVE CONTRACT, not a framework detail — every caller needs to agree on the
 * same numbers, and a bridge that forgot to cap would silently ship a log the collector then had
 * to reject or mangle instead of the bridge shaping it politely.
 *
 * The caps TRUNCATE rather than drop: a log line cut to 1024 bytes is still a log line a human
 * can act on, where a dropped record is a gap in the timeline nobody can explain later.
 */
final class LogCapture
{
    private const MAX_BODY_BYTES = 1024;
    private const MAX_ATTRIBUTES = 16;

    // Mirrors Span::MAX_VALUE_LENGTH: generous for one attribute value, still bounded so a
    // single oversized context entry cannot dominate the record.
    private const MAX_ATTRIBUTE_VALUE_BYTES = 512;
    private const MAX_ATTRIBUTE_KEY_BYTES = 128;

    /**
     * Send one log record. Fail-open and a no-op with no extension loaded — NativeExtension::
     * captureLog() already guards that, so this class only owns the shaping in front of it.
     *
     * @param array<string, mixed> $attributes
     */
    public static function send(string $psr3Level, string $body, array $attributes): void
    {
        $severity = Severity::fromPsr3($psr3Level);
        NativeExtension::captureLog(
            $severity['text'],
            $severity['number'],
            self::capBody($body),
            self::capAttributes($attributes),
        );
    }

    private static function capBody(string $body): string
    {
        if (strlen($body) <= self::MAX_BODY_BYTES) {
            return $body;
        }
        // Byte-safe, not character-safe: the native side's own cap is byte-oriented, and a
        // multibyte-aware trim here would just let the .so re-truncate mid-character anyway.
        return substr($body, 0, self::MAX_BODY_BYTES);
    }

    /**
     * @param array<string, mixed> $attributes
     * @return array<string, string>
     */
    private static function capAttributes(array $attributes): array
    {
        $capped = [];
        foreach ($attributes as $key => $value) {
            if (!is_string($key) || $key === '' || !is_scalar($value)) {
                continue;
            }
            if (count($capped) >= self::MAX_ATTRIBUTES) {
                break;
            }
            $safeKey = substr($key, 0, self::MAX_ATTRIBUTE_KEY_BYTES);
            $capped[$safeKey] = substr((string) $value, 0, self::MAX_ATTRIBUTE_VALUE_BYTES);
        }

        return $capped;
    }
}
