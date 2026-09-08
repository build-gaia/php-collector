<?php

declare(strict_types=1);

/**
 * Standalone verification for the Monolog handler + PSR-3 decorator bridges.
 *
 * Neither monolog/monolog nor psr/log is a dependency of this package (zero-runtime-deps), so
 * this test:
 *
 *   1. First proves BOTH bridge files load cleanly with neither library present at all — the
 *      exact "safely autoloadable when Monolog/psr-log are absent" requirement — by requiring
 *      them in a bare child process where nothing has defined Monolog\* or Psr\Log\* yet.
 *   2. Then, in THIS process, defines tiny stand-ins for the two Monolog record shapes (v2
 *      array-based, v3 LogRecord-object-based) and for Psr\Log\LoggerInterface/LoggerTrait,
 *      loads the bridges against them, and exercises the actual capture path: level mapping,
 *      exception-context extraction, and forwarding to an inner PSR-3 logger.
 *   3. Finally unit-tests LogCapture's own truncation caps (1024-byte body, 16 attributes)
 *      directly via reflection, since those are the exact bytes the native seam contracts on.
 *
 * Every namespace below is declared with the BRACE form (`namespace X { ... }`) rather than the
 * usual one-statement-per-file form, because this single file legitimately needs to declare
 * fixtures into several different namespaces (Monolog\*, Psr\Log\*) alongside its own test code
 * — PHP forbids mixing the two declaration styles in one file, so brace form is the only one
 * that fits.
 *
 * No PHPUnit, no vendor/: run with `php api/tests/monolog-psr3-case.php`.
 */

namespace Chronos\Collector\Tests {
    $root = __DIR__.'/..';

    spl_autoload_register(static function (string $class) use ($root): void {
        $prefix = 'Chronos\\Collector\\';
        if (!str_starts_with($class, $prefix)) {
            return;
        }
        $path = $root.'/src/'.str_replace('\\', '/', substr($class, strlen($prefix))).'.php';
        if (is_file($path)) {
            require $path;
        }
    });

    $failures = 0;

    function check(bool $condition, string $message): void
    {
        global $failures;
        if (!$condition) {
            ++$failures;
            fwrite(STDERR, "FAIL: {$message}\n");
        } else {
            fwrite(STDOUT, "PASS: {$message}\n");
        }
    }

    // --- 1. Absence: both files must load with no Monolog / psr/log defined anywhere. -------

    $absenceScript = <<<'PHP'
        <?php
        require $argv[1];
        require $argv[2];
        $handlerDeclared = class_exists('Chronos\Collector\Framework\Monolog\ChronosHandler', false);
        $loggerDeclared = class_exists('Chronos\Collector\Framework\Psr3\ChronosLogger', false);
        fwrite(STDOUT, ($handlerDeclared ? '1' : '0').($loggerDeclared ? '1' : '0'));
        PHP;

    $absenceScriptPath = tempnam(sys_get_temp_dir(), 'chronos-absence-');
    file_put_contents($absenceScriptPath, $absenceScript);
    $handlerFile = $root.'/src/Framework/Monolog/ChronosHandler.php';
    $loggerFile = $root.'/src/Framework/Psr3/ChronosLogger.php';
    $command = escapeshellarg(PHP_BINARY).' '.escapeshellarg($absenceScriptPath)
        .' '.escapeshellarg($handlerFile).' '.escapeshellarg($loggerFile);
    exec($command, $output, $exitCode);
    unlink($absenceScriptPath);
    check($exitCode === 0, 'both bridge files require() cleanly with no Monolog/psr-log present');
    check(($output[0] ?? '') === '00', 'neither ChronosHandler nor ChronosLogger is declared when their library is absent');
}

// --- 2a. Monolog v2 (array record) shape. ------------------------------------------------

namespace Monolog\Handler {
    abstract class AbstractProcessingHandler
    {
        public function __construct(int $level = 100, bool $bubble = true)
        {
        }

        public function handle($record): bool
        {
            // Real Monolog stamps a 'formatted' slot before writing: an array key for the v2
            // shape, a public property for the v3 LogRecord object.
            if (is_array($record)) {
                $record['formatted'] = '';
            } elseif (is_object($record) && property_exists($record, 'formatted')) {
                $record->formatted = '';
            }
            $this->write($record);

            return false;
        }

        abstract protected function write($record): void;
    }
}

namespace Chronos\Collector\Tests {
    use Chronos\Collector\Framework\Monolog\ChronosHandler;

    $handler = new ChronosHandler();
    $exception = new \Exception('boom');
    // Real Monolog 2 array shape: keys are 'message', 'context', 'level', 'level_name', ...
    $threw = null;
    try {
        $handler->handle([
            'message' => 'v2 body',
            'context' => ['exception' => $exception],
            'level' => 400,
            'level_name' => 'WARNING',
        ]);
    } catch (\Throwable $error) {
        $threw = $error;
    }
    check($threw === null, 'ChronosHandler::handle() does not throw for a Monolog 2 array record (native ext absent)');

    $normalise = new \ReflectionMethod(ChronosHandler::class, 'normalise');
    [$level, $body, $context] = $normalise->invoke(null, [
        'message' => 'v2 body',
        'context' => ['exception' => $exception],
        'level' => 400,
        'level_name' => 'WARNING',
    ]);
    check($level === 'warning', 'v2 record level_name WARNING normalises to psr-3 "warning"');
    check($body === 'v2 body', 'v2 record message carries through as the body');
    check(($context['exception'] ?? null) === $exception, 'v2 record context.exception survives normalisation');

    $exceptionAttributes = new \ReflectionMethod(ChronosHandler::class, 'exceptionAttributes');
    $attributes = $exceptionAttributes->invoke(null, $context);
    check(($attributes['exception.type'] ?? null) === \Exception::class, 'exception.type is set from context.exception');
    check(($attributes['exception.message'] ?? null) === 'boom', 'exception.message is set from context.exception');
}

// --- 2b. Monolog v3 (LogRecord object) shape. --------------------------------------------

namespace Monolog {
    final class FakeLevel
    {
        public function __construct(private readonly string $psr3Name)
        {
        }

        public function toPsrLogLevel(): string
        {
            return $this->psr3Name;
        }
    }

    final class LogRecord
    {
        /** @param array<mixed> $context */
        public function __construct(
            public readonly FakeLevel $level,
            public readonly string $message,
            public readonly array $context = [],
        ) {
        }
    }
}

namespace Chronos\Collector\Tests {
    use Chronos\Collector\Framework\Monolog\ChronosHandler;
    use Monolog\FakeLevel;
    use Monolog\LogRecord;

    $handler = new ChronosHandler();
    $threw = null;
    try {
        $handler->handle(new LogRecord(new FakeLevel('error'), 'v3 body'));
    } catch (\Throwable $error) {
        $threw = $error;
    }
    check($threw === null, 'ChronosHandler::handle() does not throw for a Monolog 3 LogRecord (native ext absent)');

    $normalise = new \ReflectionMethod(ChronosHandler::class, 'normalise');
    [$level, $body, $context] = $normalise->invoke(null, new LogRecord(new FakeLevel('error'), 'v3 body', ['k' => 'v']));
    check($level === 'error', 'v3 LogRecord level maps via toPsrLogLevel()');
    check($body === 'v3 body', 'v3 LogRecord message carries through as the body');
    check($context === ['k' => 'v'], 'v3 LogRecord context carries through unchanged');
}

// --- 3. PSR-3 decorator: forwards to inner logger AND captures without throwing. ---------

namespace Psr\Log {
    interface LoggerInterface
    {
        public function emergency($message, array $context = []): void;
        public function alert($message, array $context = []): void;
        public function critical($message, array $context = []): void;
        public function error($message, array $context = []): void;
        public function warning($message, array $context = []): void;
        public function notice($message, array $context = []): void;
        public function info($message, array $context = []): void;
        public function debug($message, array $context = []): void;
        public function log($level, $message, array $context = []): void;
    }

    trait LoggerTrait
    {
        public function emergency($message, array $context = []): void
        {
            $this->log('emergency', $message, $context);
        }

        public function alert($message, array $context = []): void
        {
            $this->log('alert', $message, $context);
        }

        public function critical($message, array $context = []): void
        {
            $this->log('critical', $message, $context);
        }

        public function error($message, array $context = []): void
        {
            $this->log('error', $message, $context);
        }

        public function warning($message, array $context = []): void
        {
            $this->log('warning', $message, $context);
        }

        public function notice($message, array $context = []): void
        {
            $this->log('notice', $message, $context);
        }

        public function info($message, array $context = []): void
        {
            $this->log('info', $message, $context);
        }

        public function debug($message, array $context = []): void
        {
            $this->log('debug', $message, $context);
        }
    }
}

namespace Chronos\Collector\Tests {
    use Chronos\Collector\Framework\Psr3\ChronosLogger;
    use Psr\Log\LoggerInterface;

    final class RecordingLogger implements LoggerInterface
    {
        use \Psr\Log\LoggerTrait;

        /** @var list<array{0: mixed, 1: mixed, 2: array<mixed>}> */
        public array $received = [];

        public function log($level, $message, array $context = []): void
        {
            $this->received[] = [$level, $message, $context];
        }
    }

    $inner = new RecordingLogger();
    $logger = new ChronosLogger($inner);
    $innerException = new \Exception('inner boom');
    $threw = null;
    try {
        $logger->warning('psr-3 body', ['exception' => $innerException]);
    } catch (\Throwable $error) {
        $threw = $error;
    }
    check($threw === null, 'ChronosLogger::log() does not throw with native ext absent');
    check(count($inner->received) === 1, 'the inner logger received exactly one forwarded call');
    check(($inner->received[0][0] ?? null) === 'warning', 'the inner logger receives the same PSR-3 level');
    check(($inner->received[0][1] ?? null) === 'psr-3 body', 'the inner logger receives the same message');
    check(($inner->received[0][2]['exception'] ?? null) === $innerException, 'the inner logger receives the original context untouched');

    // No inner logger at all is a valid, still-safe configuration.
    $standalone = new ChronosLogger();
    $threw = null;
    try {
        $standalone->error('no inner logger');
    } catch (\Throwable $error) {
        $threw = $error;
    }
    check($threw === null, 'ChronosLogger works with no inner logger configured');
}

// --- 4. LogCapture's own truncation caps. -------------------------------------------------

namespace Chronos\Collector\Tests {
    use Chronos\Collector\Service\LogCapture;

    $capBody = new \ReflectionMethod(LogCapture::class, 'capBody');
    $longBody = str_repeat('a', 2000);
    $cappedBody = $capBody->invoke(null, $longBody);
    check(strlen($cappedBody) === 1024, 'capBody truncates an oversized body to exactly 1024 bytes');
    $shortBody = 'small';
    check($capBody->invoke(null, $shortBody) === $shortBody, 'capBody leaves a body under the cap untouched');

    $capAttributes = new \ReflectionMethod(LogCapture::class, 'capAttributes');
    $manyAttributes = [];
    for ($i = 0; $i < 20; ++$i) {
        $manyAttributes['key'.$i] = 'value'.$i;
    }
    $cappedAttributes = $capAttributes->invoke(null, $manyAttributes);
    check(count($cappedAttributes) === 16, 'capAttributes truncates to at most 16 attributes, never drops the record');

    $oversizedValue = ['k' => str_repeat('x', 1000)];
    $cappedValue = $capAttributes->invoke(null, $oversizedValue);
    check(strlen($cappedValue['k']) === 512, 'capAttributes bounds a single oversized attribute value');

    $nonScalar = ['k' => 'ok', 'bad' => ['nested' => 'array']];
    $filtered = $capAttributes->invoke(null, $nonScalar);
    check(array_key_exists('k', $filtered) && !array_key_exists('bad', $filtered), 'capAttributes drops non-scalar values without breaking the rest');

    fwrite(STDOUT, sprintf("\n%d failure(s)\n", $failures));
    exit($failures === 0 ? 0 : 1);
}
