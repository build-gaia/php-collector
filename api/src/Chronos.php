<?php

declare(strict_types=1);

namespace Chronos\Collector;

use Chronos\Collector\Service\SpanManager;

/**
 * The single public entry point application code reaches for manual instrumentation:
 * $chronos->span->create('name'). Bound into the Laravel and Symfony DI containers so both
 * frameworks resolve the same request-scoped span stack; framework-agnostic code (Symfony 1,
 * plain PHP) can reach the same stack by constructing this class directly, since span is a
 * plain public property and SpanManager itself carries no per-instance state.
 */
final class Chronos
{
    public readonly SpanManager $span;

    public function __construct()
    {
        $this->span = new SpanManager();
    }
}
