<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * Maps each framework's own log level vocabulary onto the language-agnostic Chronos log contract
 * severity pair (a stable text name plus the OpenTelemetry severity number). Keeping this in one
 * place means every framework adapter — and, in time, every other language's collector — agrees
 * on what "error" means without duplicating the table.
 */
final class Severity
{
    /** @return array{text: string, number: int} */
    public static function fromPsr3(string $level): array
    {
        switch (strtolower(trim($level))) {
            case 'debug':
                return self::pair('DEBUG', 5);
            case 'info':
            case 'notice':
                return self::pair('INFO', 9);
            case 'warning':
                return self::pair('WARN', 13);
            case 'error':
                return self::pair('ERROR', 17);
            case 'critical':
            case 'alert':
            case 'emergency':
                return self::pair('FATAL', 21);
            default:
                return self::pair('UNSPECIFIED', 0);
        }
    }

    /**
     * Symfony 1's sfLogger priority constants: EMERG=0, ALERT=1, CRIT=2, ERR=3, WARNING=4,
     * NOTICE=5, INFO=6, DEBUG=7. Anything outside that range is treated as unspecified.
     *
     * @return array{text: string, number: int}
     */
    public static function fromSymfony1(int $priority): array
    {
        switch ($priority) {
            case 0:
            case 1:
            case 2:
                return self::pair('FATAL', 21);
            case 3:
                return self::pair('ERROR', 17);
            case 4:
                return self::pair('WARN', 13);
            case 5:
            case 6:
                return self::pair('INFO', 9);
            case 7:
                return self::pair('DEBUG', 5);
            default:
                return self::pair('UNSPECIFIED', 0);
        }
    }

    /** @return array{text: string, number: int} */
    private static function pair(string $text, int $number): array
    {
        return ['text' => $text, 'number' => $number];
    }
}
