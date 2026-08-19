<?php

declare(strict_types=1);

namespace Chronos\Collector\Service;

/**
 * Resolves the running application's release version once per process so error and trace
 * views can attribute a fault to the deploy that introduced it. Precedence, most to least
 * explicit:
 *
 *   1. CHRONOS_APP_VERSION  — operator-set override, wins over anything inferred.
 *   2. CHRONOS_IMAGE_TAG / IMAGE_TAG — the container image tag injected by the build/deploy
 *      pipeline (the tag is usually already pinned to a release or commit).
 *   3. GIT_COMMIT / SOURCE_COMMIT — a build-time commit exposed by CI (Jenkins, Docker Hub
 *      autobuild, ...), surfaced as a short hash.
 *
 * Resolves to null (the attribute is omitted) when nothing is set, keeping the collector
 * zero-config-safe. Reads only environment already present in the process across the same
 * getenv() / $_ENV / $_SERVER mechanisms the rest of the collector relies on — it never
 * shells out to git, so there is no per-request cost.
 */
final class AppVersion
{
    /** Longest version string stamped; image tags and refs beyond this are truncated. */
    private const MAX_LENGTH = 128;

    /** Environment variables consulted, in precedence order, grouped by resolution stage. */
    private const EXPLICIT = ['CHRONOS_APP_VERSION'];
    private const IMAGE_TAG = ['CHRONOS_IMAGE_TAG', 'IMAGE_TAG'];
    private const GIT_COMMIT = ['GIT_COMMIT', 'SOURCE_COMMIT'];

    private static bool $memoised = false;

    private static ?string $version = null;

    /**
     * The resolved version, computed once and cached for the remainder of the process.
     * Returns null when no version could be resolved, so callers omit the attribute.
     */
    public static function resolve(): ?string
    {
        if (!self::$memoised) {
            self::$version = self::detect();
            self::$memoised = true;
        }

        return self::$version;
    }

    /**
     * Discards the memoised value so the next resolve() re-reads the environment. Intended for
     * the verification harness, which exercises the precedence chain under different env sets;
     * production code resolves exactly once.
     */
    public static function reset(): void
    {
        self::$memoised = false;
        self::$version = null;
    }

    private static function detect(): ?string
    {
        $explicit = self::firstPresent(self::EXPLICIT);
        if ($explicit !== null) {
            return self::bound($explicit);
        }
        $imageTag = self::firstPresent(self::IMAGE_TAG);
        if ($imageTag !== null) {
            return self::bound($imageTag);
        }
        $commit = self::firstPresent(self::GIT_COMMIT);
        if ($commit !== null) {
            return self::bound(self::shortCommit($commit));
        }

        return null;
    }

    /**
     * First non-empty value among the given variable names, read across getenv(), $_ENV and
     * $_SERVER — mirroring how frameworks populate their environment (Symfony Dotenv via getenv,
     * Laravel phpdotenv via $_ENV / $_SERVER without putenv()).
     *
     * @param list<string> $names
     */
    private static function firstPresent(array $names): ?string
    {
        foreach ($names as $name) {
            $value = getenv($name);
            if (is_string($value) && $value !== '') {
                return $value;
            }
            foreach ([$_ENV, $_SERVER] as $bag) {
                if (isset($bag[$name]) && is_scalar($bag[$name]) && (string) $bag[$name] !== '') {
                    return (string) $bag[$name];
                }
            }
        }

        return null;
    }

    /** Shortens a full 40/64-char git object hash to a 12-char prefix; other refs pass through. */
    private static function shortCommit(string $commit): string
    {
        $trimmed = trim($commit);
        if (preg_match('/^[a-f0-9]{40}$|^[a-f0-9]{64}$/D', $trimmed) === 1) {
            return substr($trimmed, 0, 12);
        }

        return $trimmed;
    }

    /** Trims and length-caps a resolved version, collapsing an empty result back to null. */
    private static function bound(string $value): ?string
    {
        $trimmed = trim($value);
        if ($trimmed === '') {
            return null;
        }

        return substr($trimmed, 0, self::MAX_LENGTH);
    }
}
