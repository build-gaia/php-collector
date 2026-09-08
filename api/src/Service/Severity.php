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
    /**
     * PSR-3's eight levels onto OpenTelemetry's documented syslog mapping — each level gets
     * its OWN severityNumber, because the top of the scale is exactly where flattening hurts:
     * an alert rule filtering severityNumber >= 22 to separate true emergencies from mere
     * criticals must not silently match nothing because all three collapsed onto 21. The rank
     * texts (INFO2, FATAL2…) are OTel's own short names for those numbers; the engine keys on
     * the number whenever it is in 1..24 and only falls back to the text.
     *
     * @return array{text: string, number: int}
     */
    public static function fromPsr3(string $level): array
    {
        switch (strtolower(trim($level))) {
            case 'debug':
                return self::pair('DEBUG', 5);
            case 'info':
                return self::pair('INFO', 9);
            case 'notice':
                return self::pair('INFO2', 10);
            case 'warning':
                return self::pair('WARN', 13);
            case 'error':
                return self::pair('ERROR', 17);
            case 'critical':
                return self::pair('FATAL2', 22);
            case 'alert':
                return self::pair('FATAL3', 23);
            case 'emergency':
                return self::pair('FATAL4', 24);
            default:
                return self::pair('UNSPECIFIED', 0);
        }
    }

    /**
     * Symfony 1's sfLogger priority constants: EMERG=0, ALERT=1, CRIT=2, ERR=3, WARNING=4,
     * NOTICE=5, INFO=6, DEBUG=7. Anything outside that range is treated as unspecified.
     * These ARE syslog severities under other names, so they take the same distinct
     * numbers as fromPsr3() above — the two tables must agree on what "emergency" means.
     *
     * @return array{text: string, number: int}
     */
    public static function fromSymfony1(int $priority): array
    {
        switch ($priority) {
            case 0:
                return self::pair('FATAL4', 24);
            case 1:
                return self::pair('FATAL3', 23);
            case 2:
                return self::pair('FATAL2', 22);
            case 3:
                return self::pair('ERROR', 17);
            case 4:
                return self::pair('WARN', 13);
            case 5:
                return self::pair('INFO2', 10);
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
