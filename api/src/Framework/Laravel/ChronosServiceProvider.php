<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Chronos;
use Chronos\Collector\Service\NativeExtension;
use Illuminate\Contracts\Http\Kernel;
use Illuminate\Support\ServiceProvider;
use Throwable;

final class ChronosServiceProvider extends ServiceProvider
{
    public function register(): void
    {
        $this->app->singleton('chronos', static fn (): Chronos => new Chronos());
    }

    public function boot(): void
    {
        // enabled() is the process-level master switch (extension loaded AND
        // CHRONOS_PHP_ENABLED on): with the .so baked into a fleet image but the
        // collector off, this provider registers nothing at all.
        if (!NativeExtension::enabled()) {
            return;
        }

        try {
            $this->app->make(Kernel::class)->pushMiddleware(RecordChronosRequest::class);
        } catch (Throwable) {
        }

        RichTelemetryHooks::install();
    }
}
