<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony1;

use Chronos\Collector\Service\NativeExtension;
use Chronos\Collector\Service\Severity;
use Throwable;

/**
 * Bridges Symfony 1's event-based logging to the native Rust collector.
 * Connected to the event dispatcher by ChronosFilter at request start.
 */
final class ChronosLogListener
{
    private const SF_PRIORITY_INFO = 6;

    public function onLog(object $event): void
    {
        try {
            $parameters = self::parameters($event);
            $priority = isset($parameters['priority']) && is_numeric($parameters['priority'])
                ? (int) $parameters['priority']
                : self::SF_PRIORITY_INFO;
            $severity = Severity::fromSymfony1($priority);

            foreach ($parameters as $key => $message) {
                if (!is_int($key)) {
                    continue;
                }
                if (is_string($message) && $message !== '') {
                    NativeExtension::captureLog($severity['text'], $severity['number'], $message, []);
                }
            }
        } catch (Throwable) {
        }
    }

    public function onException(object $event): void
    {
        try {
            $exception = method_exists($event, 'getSubject') ? $event->getSubject() : null;
            if (!$exception instanceof Throwable) {
                return;
            }
            $severity = Severity::fromSymfony1(2);
            NativeExtension::captureLog(
                $severity['text'],
                $severity['number'],
                $exception->getMessage(),
                [
                    'exception.type' => get_class($exception),
                    'exception.code' => (string) $exception->getCode(),
                ],
            );
        } catch (Throwable) {
        }
    }

    /** @return array<int|string, mixed> */
    private static function parameters(object $event): array
    {
        if (method_exists($event, 'getParameters')) {
            $parameters = $event->getParameters();
            if (is_array($parameters)) {
                return $parameters;
            }
        }
        return [];
    }
}
