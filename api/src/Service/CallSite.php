<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

use Throwable;

/**
 * Call-site provenance for spans and activity catalogs.
 *
 * Two readings from one stack walk:
 *
 * 1. The first application frame (`code.filepath` / `code.lineno` / `code.function`)
 *    — not under an excluded tree such as `/vendor/`, not the collector. This is
 *    the editor jump, and the only durable link from a queued job back to the
 *    request that dispatched it.
 * 2. The stack read outwards FROM that frame (`code.stacktrace`). The innermost
 *    frames of an instrumented sink are framework plumbing — a Laravel query
 *    arrives under Dispatcher → invokeListeners → dispatch → event → logQuery,
 *    five frames that say nothing the span's own name did not. So the stack
 *    starts at the boundary: the frame where userland enters the excluded tree,
 *    which carries the vendor entry point as its function and the first-party
 *    file as its location, followed by the userland callers above it.
 *    A stack with no first-party frame at all (framework bootstrap, a queue
 *    worker) keeps the nearest frames, because there is nothing better to show.
 *
 * `debug_backtrace` is IGNORE_ARGS and bounded. It is not gated on the counted
 * profiler (`profile_deterministic`): that is per-function totals from the Zend
 * observer, not a stack at an I/O sink. DST recording is the full call path and
 * is a different mechanism again.
 *
 * Fail-open throughout — a call site that cannot be resolved is a null, never an
 * exception raised inside instrumentation.
 */
final class CallSite
{
    /**
     * How deep to walk. A dispatch can sit under a dozen framework frames; past
     * this the walk costs more than the answer is worth.
     */
    private const MAX_FRAMES = 40;

    /** Frames kept on `code.stacktrace` — Debugbar's per-query depth. */
    public const STACK_LIMIT = 5;

    /**
     * Path fragments that mark a frame as somebody else's code. A file under one
     * of these is never the editor jump, and never starts the recorded stack.
     */
    private const EXCLUDED_PATH_FRAGMENTS = ['/vendor/'];

    /**
     * @return array{0: ?string, 1: int, 2: ?string} file, line, function
     */
    public static function firstApplicationFrame(): array
    {
        [$file, $line, $function] = self::capture();

        return [$file, $line, $function];
    }

    /**
     * One walk: first-party call site plus the bounded stack above it.
     *
     * @return array{0: ?string, 1: int, 2: ?string, 3: ?string} file, line, function, stacktrace JSON
     */
    public static function capture(int $stackLimit = self::STACK_LIMIT): array
    {
        $limit = max(0, $stackLimit);
        try {
            $frames = debug_backtrace(DEBUG_BACKTRACE_IGNORE_ARGS, self::MAX_FRAMES);
        } catch (Throwable) {
            return [null, 0, null, null];
        }

        $frames = array_values(array_filter(
            $frames,
            static fn (array $frame): bool => !self::isCollectorFrame($frame),
        ));

        $entry = null;
        foreach ($frames as $index => $frame) {
            if (self::isApplicationPath($frame['file'] ?? null)) {
                $entry = $index;
                break;
            }
        }

        $file = null;
        $line = 0;
        $function = null;
        if ($entry !== null) {
            $file = (string) $frames[$entry]['file'];
            $line = (int) ($frames[$entry]['line'] ?? 0);
            $function = self::frameFunction($frames[$entry + 1] ?? null);
        }

        $recent = [];
        $count = count($frames);
        for ($index = $entry ?? 0; $index < $count && count($recent) < $limit; $index++) {
            $normalised = self::normaliseFrame($frames[$index]);
            if ($normalised !== []) {
                $recent[] = $normalised;
            }
        }

        $json = null;
        if ($recent !== []) {
            $encoded = json_encode($recent, JSON_UNESCAPED_SLASHES);
            $json = is_string($encoded) ? $encoded : null;
        }

        return [$file, $line, $function, $json];
    }

    /** Stamp `code.*` plus `code.stacktrace` onto an open span. */
    public static function applyToSpan(Span $span, int $stackLimit = self::STACK_LIMIT): void
    {
        if ($span->isVoid()) {
            return;
        }
        [$file, $line, $function, $stacktrace] = self::capture($stackLimit);
        if ($file !== null && $file !== '') {
            $span->add('code.filepath', $file);
            if ($line > 0) {
                $span->add('code.lineno', (string) $line);
            }
        }
        if ($function !== null && $function !== '') {
            $span->add('code.function', $function);
        }
        if ($stacktrace !== null && $stacktrace !== '') {
            $span->add('code.stacktrace', $stacktrace, Span::MAX_TEXT_LENGTH);
        }
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

    /** A frame located in first-party code — the boundary the stack starts at. */
    private static function isApplicationPath(mixed $path): bool
    {
        if (!is_string($path) || $path === '') {
            return false;
        }
        foreach (self::EXCLUDED_PATH_FRAGMENTS as $fragment) {
            if (str_contains($path, $fragment)) {
                return false;
            }
        }

        return true;
    }

    /**
     * @param array<string, mixed> $frame
     */
    private static function isCollectorFrame(array $frame): bool
    {
        $class = $frame['class'] ?? null;

        return is_string($class) && str_starts_with($class, 'Chronos\\Collector\\');
    }

    /**
     * @param array<string, mixed> $frame
     * @return array<string, string|int>
     */
    private static function normaliseFrame(array $frame): array
    {
        $out = [];
        $function = is_string($frame['function'] ?? null) ? $frame['function'] : '';
        if ($function !== '') {
            $out['function'] = $function;
        }
        $class = is_string($frame['class'] ?? null) ? $frame['class'] : '';
        if ($class !== '') {
            $out['class'] = $class;
        }
        $type = is_string($frame['type'] ?? null) ? $frame['type'] : '';
        if ($type !== '') {
            $out['type'] = $type;
        }
        $file = is_string($frame['file'] ?? null) ? $frame['file'] : '';
        if ($file !== '') {
            $out['file'] = $file;
        }
        $line = $frame['line'] ?? null;
        if (is_int($line) && $line > 0) {
            $out['line'] = $line;
        } elseif (is_numeric($line) && (int) $line > 0) {
            $out['line'] = (int) $line;
        }

        return $out;
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
