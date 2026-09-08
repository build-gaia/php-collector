<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Psr3;

use Chronos\Collector\Service\LogCapture;
use Throwable;

/**
 * A PSR-3 logger DECORATOR: wraps an application's existing `Psr\Log\LoggerInterface` instance
 * (Monolog's own `Logger`, a framework's container-bound logger, anything) and mirrors every
 * call into Chronos, unchanged, before forwarding it on. This is the seam for an app that logs
 * against the PSR-3 interface directly rather than through a handler-based library — swap the
 * bound implementation for `new ChronosLogger($originalLogger)` and every existing `$logger->
 * warning(...)` call site starts shipping to Chronos with no other code change.
 *
 * psr/log is NOT a dependency of this package (zero-runtime-dependency constraint), so — same
 * technique as ChronosHandler next door — the whole class declaration sits behind an
 * `interface_exists()` guard rather than a hard `use`+`implements`, so this file loads cleanly
 * whether or not psr/log is on the app's classmap. `LoggerTrait` ships in the SAME psr/log
 * package as `LoggerInterface`, so guarding on the interface's presence is sufficient to know
 * the trait is there too; it supplies the eight level-named convenience methods (warning(),
 * error(), ...) so this class only has to implement `log()` itself.
 *
 * The inner logger is OPTIONAL: with none given, this behaves as a plain Chronos-only logger —
 * useful for a fresh container binding where nothing was logging before.
 *
 * `$level`/`$message` are deliberately left untyped rather than copying whichever psr/log
 * version's exact signature (`string` in 1.x/2.x, `string|\Stringable` in 3.x, `$level` as
 * `mixed` since 3.x) — an untyped parameter is a valid (wider) override of any typed interface
 * parameter, so this one implementation satisfies every psr/log major without knowing which is
 * installed.
 */
if (interface_exists(\Psr\Log\LoggerInterface::class)) {
    final class ChronosLogger implements \Psr\Log\LoggerInterface
    {
        use \Psr\Log\LoggerTrait;

        public function __construct(private readonly ?\Psr\Log\LoggerInterface $inner = null)
        {
        }

        public function log($level, $message, array $context = []): void
        {
            try {
                $psr3Level = is_string($level) ? $level : (string) $level;
                $body = is_scalar($message) || $message instanceof \Stringable ? (string) $message : '';
                LogCapture::send($psr3Level, $body, self::exceptionAttributes($context));
            } catch (Throwable) {
                // Capture must never be able to break the application's real logging path.
            }

            $this->inner?->log($level, $message, $context);
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
