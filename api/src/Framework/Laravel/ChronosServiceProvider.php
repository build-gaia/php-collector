<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Laravel;

use Chronos\Collector\Chronos;
use Chronos\Collector\Service\NativeExtension;
use Illuminate\Contracts\Debug\ExceptionHandler;
use Illuminate\Contracts\Http\Kernel;
use Illuminate\Contracts\View\Engine;
use Illuminate\Support\ServiceProvider;
use Throwable;

final class ChronosServiceProvider extends ServiceProvider
{
    /**
     * The engines Laravel registers by name. There is no way to enumerate them —
     * `EngineResolver::$resolvers` is protected — so the known set is tried and
     * whatever is not registered is skipped.
     */
    private const VIEW_ENGINES = ['blade', 'php', 'file'];

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

        $this->observeBootCompletion();
        $this->instrumentViewEngines();
        $this->captureReportedExceptions();

        QueueTelemetry::install();
        RichTelemetryHooks::install();
    }

    /**
     * Record the instant the container finishes booting.
     *
     * This callback is the only observable end of framework bootstrap, and it
     * fires before any middleware runs — which is exactly why bootstrap cannot be
     * a phase mark and is emitted as a span instead. See BootTiming.
     */
    private function observeBootCompletion(): void
    {
        try {
            $this->app->booted(function (): void {
                $loaded = 0;
                try {
                    $loaded = count($this->app->getLoadedProviders());
                } catch (Throwable) {
                }
                BootTiming::markBooted($loaded);
            });
        } catch (Throwable) {
        }
    }

    /**
     * Wrap the registered view engines so each template render becomes a span.
     *
     * Deferred through `afterResolving` rather than resolving the view system
     * here: an API-only service never renders a template, and forcing the view
     * factory to boot so it could be instrumented would cost every one of its
     * requests for telemetry none of them produce.
     */
    private function instrumentViewEngines(): void
    {
        try {
            $wrap = static function (mixed $resolver): void {
                if (!is_object($resolver) || !method_exists($resolver, 'resolve') || !method_exists($resolver, 'register')) {
                    return;
                }
                foreach (self::VIEW_ENGINES as $name) {
                    try {
                        $engine = $resolver->resolve($name);
                    } catch (Throwable) {
                        // Not registered in this application; nothing to wrap.
                        continue;
                    }
                    if (!$engine instanceof Engine || $engine instanceof ChronosViewEngine) {
                        continue;
                    }
                    $wrapped = new ChronosViewEngine($engine);
                    $resolver->register($name, static fn (): Engine => $wrapped);
                }
            };

            $this->app->afterResolving('view.engine.resolver', $wrap);
            if ($this->app->resolved('view.engine.resolver')) {
                $wrap($this->app->make('view.engine.resolver'));
            }
        } catch (Throwable) {
        }
    }

    /**
     * Put every reported exception on the trace, not only the one that reached
     * the response.
     *
     * The callback deliberately returns nothing: a `reportable` handler that
     * returns false stops Laravel's own reporting, and instrumentation that
     * silently disabled the application's logging would be a bug of the worst
     * kind — one found only when someone goes looking for a log that was never
     * written.
     */
    private function captureReportedExceptions(): void
    {
        try {
            $handler = $this->app->make(ExceptionHandler::class);
            if (!is_object($handler) || !method_exists($handler, 'reportable')) {
                return;
            }
            $handler->reportable(static function (Throwable $exception): void {
                ExceptionCapture::record($exception);
            });
        } catch (Throwable) {
        }
    }
}
