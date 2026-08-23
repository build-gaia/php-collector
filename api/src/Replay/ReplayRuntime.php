<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * Process-level wiring for replay mode: discover the recording once, keep the session, settle
 * the exit code at shutdown.
 *
 * Deliberately the only stateful thing in this module. The protocol engine takes an environment
 * array and answers lookups; everything that makes a PHP PROCESS a replay — reading the real
 * environment, registering a shutdown handler, overriding the exit code — is confined here so a
 * Node or Go runtime can reproduce the contract without reproducing any of it.
 */
final class ReplayRuntime
{
    public const IMPLEMENTATION = 'chronos-collector-php replay 1.0';

    private static ?ReplaySession $session = null;

    private static bool $booted = false;

    /**
     * Replaceable process terminator. Production exits; a conformance runner or a unit test
     * substitutes something observable, because a test that took the interpreter down with it
     * could not assert on what happened.
     *
     * @var callable(int): void|null
     */
    private static $terminator = null;

    /**
     * Arm replay mode for this process. A no-op — and no report — when
     * CHRONOS_REPLAY_RECORDING is absent, which is what keeps one image usable for both
     * recording and replay.
     *
     * @param array<string, string>|null $environment defaults to the real process environment
     */
    public static function boot(?array $environment = null): ?ReplaySession
    {
        if (self::$booted) {
            return self::$session;
        }
        self::$booted = true;
        $resolved = $environment ?? self::processEnvironment();
        self::$session = ReplaySession::begin($resolved);
        if (self::$session === null) {
            return null;
        }
        // Registered even when discovery already failed: the session has written its report,
        // but the process still has to exit 91 rather than whatever the workload would have
        // returned, and the workload is not supposed to have run at all.
        register_shutdown_function(static function (): void {
            self::settle();
        });

        return self::$session;
    }

    public static function session(): ?ReplaySession
    {
        return self::$session;
    }

    public static function active(): bool
    {
        return self::$session?->isRunning() === true;
    }

    /**
     * Finish the replay and, unless the outcome is conformant, force the process exit code.
     *
     * The report is authoritative and the exit code is its summary: a diverged replay must never
     * look green to the scheduler. The converse matters just as much — a CONFORMANT replay lets
     * the application's own exit code through untouched, because an application failing on its
     * own terms is the application's business and not a protocol event.
     */
    public static function settle(): void
    {
        $session = self::$session;
        if ($session === null) {
            return;
        }
        $code = $session->finish();
        if ($code !== ReplaySession::EXIT_CONFORMANT) {
            self::terminate($code);
        }
    }

    /**
     * Stop the replay now, because a lookup could not be answered honestly.
     *
     * Not throwable-and-catchable in production: the application must not be able to keep going
     * past a call the recording cannot answer, so the report is written and the process ends.
     */
    public static function abort(Answer $answer): void
    {
        $code = self::$session?->finish() ?? ReplaySession::EXIT_ABORTED;
        self::terminate($code);

        // Only reachable when the terminator has been replaced, since the real one does not
        // return. Making the abort observable in-process is what lets it be asserted on.
        throw new ReplayAborted($answer);
    }

    /** @param callable(int): void|null $terminator */
    public static function useTerminator(?callable $terminator): void
    {
        self::$terminator = $terminator;
    }

    /** Test seam, mirroring NativeExtension::reset(). */
    public static function reset(): void
    {
        self::$session = null;
        self::$booted = false;
        self::$terminator = null;
    }

    /**
     * The real process environment.
     *
     * getenv() with no argument is used rather than $_ENV or $_SERVER because variables_order
     * decides whether those are populated at all, and a php.ini that leaves E out would make a
     * replay silently behave as an ordinary run — the failure mode with the fewest visible
     * symptoms.
     *
     * @return array<string, string>
     */
    public static function processEnvironment(): array
    {
        $environment = [];
        foreach (getenv() as $name => $value) {
            $environment[(string) $name] = (string) $value;
        }

        return $environment;
    }

    private static function terminate(int $code): void
    {
        if (self::$terminator !== null) {
            (self::$terminator)($code);

            return;
        }
        exit($code);
    }
}
