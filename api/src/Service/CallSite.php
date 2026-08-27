<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Throwable;

/**
 * The first application frame above the collector: file, line, and the function
 * that frame was executing in.
 *
 * This is what makes an event or a queued job navigable. A job runs in another
 * process, in another trace, and the only durable link back to the request that
 * caused it is the file and line that dispatched it — so the call site is not
 * decoration on those catalogs (ADR 0024 §2, §3), it is the field that turns a
 * name into somewhere to go.
 *
 * "Application" means "not under /vendor/": the frame that dispatched an event is
 * the interesting one, not the framework machinery that carried it. Fail-open
 * throughout — a call site that cannot be resolved is a null, never an exception
 * raised inside instrumentation.
 */
final class CallSite
{
    /**
     * How deep to walk. A dispatch can sit under a dozen framework frames; past
     * this the walk costs more than the answer is worth.
     */
    private const MAX_FRAMES = 40;

    /**
     * @return array{0: ?string, 1: int, 2: ?string} file, line, function
     */
    public static function firstApplicationFrame(): array
    {
        try {
            $frames = debug_backtrace(DEBUG_BACKTRACE_IGNORE_ARGS, self::MAX_FRAMES);
            foreach ($frames as $index => $frame) {
                $file = $frame['file'] ?? null;
                if (is_string($file) && $file !== '' && !str_contains($file, '/vendor/')) {
                    return [$file, (int) ($frame['line'] ?? 0), self::frameFunction($frames[$index + 1] ?? null)];
                }
            }
        } catch (Throwable) {
        }

        return [null, 0, null];
    }

    /**
     * The call site as the `code.*` attributes the catalogs carry.
     *
     * @return array<string, string>
     */
    public static function attributes(): array
    {
        [$file, $line, $function] = self::firstApplicationFrame();
        $attributes = [];
        if ($file !== null && $file !== '') {
            $attributes['code.filepath'] = $file;
        }
        if ($line > 0) {
            $attributes['code.lineno'] = (string) $line;
        }
        if ($function !== null && $function !== '') {
            $attributes['code.function'] = $function;
        }

        return $attributes;
    }

    /**
     * The frame ABOVE the located one names the function being executed there —
     * `debug_backtrace` records file/line at the call and the callee's name one
     * frame up.
     *
     * @param array<string, mixed>|null $frame
     */
    private static function frameFunction(?array $frame): ?string
    {
        if ($frame === null) {
            return null;
        }
        $function = is_string($frame['function'] ?? null) ? $frame['function'] : '';
        if ($function === '') {
            return null;
        }
        $class = is_string($frame['class'] ?? null) ? $frame['class'] : '';

        return $class === '' ? $function : $class.'::'.$function;
    }
}
