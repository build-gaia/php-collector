<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Monolog;

use Chronos\Collector\Service\LogCapture;
use Throwable;

/**
 * A Monolog handler that forwards every record to the native Chronos collector, so any
 * application already logging through Monolog (Laravel's own default channel, or a bare
 * "monolog/monolog" install with no framework at all) ships logs with zero call-site changes
 * beyond pushing this handler onto a channel.
 *
 * monolog/monolog is NOT a dependency of this package (the zero-runtime-dependency constraint
 * in composer.json is absolute), so the class below can only be declared once Monolog's own
 * classes are known to exist — otherwise `class ChronosHandler extends AbstractProcessingHandler`
 * would fatal the moment this file is parsed for ANY application, Monolog or not.
 *
 * The guard is a runtime `class_exists()` check wrapped around the whole class declaration
 * (not a `class_exists()` check inside the class body — PHP resolves an `extends` target when
 * the class declaration statement EXECUTES, not when the file is parsed, so a false condition
 * here skips declaring the class entirely and the file loads cleanly either way). This mirrors
 * how ChronosPipeline extends Predis\Pipeline\Pipeline elsewhere in this package, except Predis
 * relies on nothing ever autoloading that class name when Predis is absent; Monolog is common
 * enough, and this handler foreseeable enough to reference speculatively (e.g. a service
 * container that lists handler classes to `class_exists()`-probe), that the explicit guard here
 * is worth the extra line.
 *
 * Monolog 2 hands `handle()`/`write()` a plain array `$record`; Monolog 3 hands it a
 * `Monolog\LogRecord` value object. AbstractProcessingHandler's OWN abstract `write()` signature
 * differs between the two majors (`array` vs `LogRecord`), so this override deliberately leaves
 * the parameter untyped — an untyped parameter is a valid (wider) override of either, and
 * because this file only ever loads against ONE concrete Monolog major per process, exactly one
 * of those parent signatures is ever in effect.
 */
if (class_exists(\Monolog\Handler\AbstractProcessingHandler::class)) {
    final class ChronosHandler extends \Monolog\Handler\AbstractProcessingHandler
    {
        /**
         * @param array<string, mixed>|\Monolog\LogRecord $record
         */
        protected function write($record): void
        {
            try {
                [$level, $body, $context] = self::normalise($record);
                LogCapture::send($level, $body, self::exceptionAttributes($context));
            } catch (Throwable) {
                // A log record must never be able to take the application down.
            }
        }

        /**
         * Reduce either record shape to (PSR-3 level name, message body, context array).
         *
         * @param array<string, mixed>|\Monolog\LogRecord $record
         * @return array{0: string, 1: string, 2: array<mixed>}
         */
        private static function normalise(mixed $record): array
        {
            if (class_exists(\Monolog\LogRecord::class) && $record instanceof \Monolog\LogRecord) {
                $level = $record->level;
                $psr3Level = is_object($level) && method_exists($level, 'toPsrLogLevel')
                    ? (string) $level->toPsrLogLevel()
                    : 'info';
                $context = is_array($record->context) ? $record->context : [];

                return [$psr3Level, (string) $record->message, $context];
            }

            // Monolog 2 shape: a plain array with 'level_name' (e.g. "WARNING"), which is
            // already the upper-cased spelling of the PSR-3 level name Severity::fromPsr3 wants.
            $record = is_array($record) ? $record : [];
            $levelName = $record['level_name'] ?? null;
            $psr3Level = is_string($levelName) ? strtolower($levelName) : 'info';
            $body = isset($record['message']) && is_scalar($record['message']) ? (string) $record['message'] : '';
            $context = isset($record['context']) && is_array($record['context']) ? $record['context'] : [];

            return [$psr3Level, $body, $context];
        }

        /**
         * @param array<mixed> $context
         * @return array<string, string>
         */
        private static function exceptionAttributes(array $context): array
        {
            $exception = $context['exception'] ?? null;
            if (!$exception instanceof Throwable) {
                return [];
            }

            return [
                'exception.type' => get_class($exception),
                'exception.message' => $exception->getMessage(),
            ];
        }
    }
}
