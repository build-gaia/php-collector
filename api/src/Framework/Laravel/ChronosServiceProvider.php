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
        try {
            $this->app->make(Kernel::class)->pushMiddleware(RecordChronosRequest::class);
        } catch (Throwable) {
        }

        if (NativeExtension::loaded()) {
            RichTelemetryHooks::install();
        }
    }
}
