<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony;

use Chronos\Collector\Chronos;
use Chronos\Collector\Service\NativeExtension;
use Symfony\Component\DependencyInjection\Compiler\PassConfig;
use Symfony\Component\DependencyInjection\ContainerBuilder;
use Symfony\Component\DependencyInjection\Reference;
use Symfony\Component\HttpKernel\Bundle\Bundle;

final class ChronosBundle extends Bundle
{
    public function build(ContainerBuilder $container): void
    {
        parent::build($container);

        // Without the native extension there is nothing for any of this to talk to,
        // so wire none of it: an application that has the composer package but no
        // .so gets its own container back, unchanged. This is the same floor
        // ChronosServiceProvider::boot() gives a Laravel application, which until
        // now Symfony did not have — every bridge below was registered regardless,
        // and only declined to act once it was already running.
        //
        // Gated on loaded() rather than enabled(): this runs at CONTAINER COMPILE
        // time and the result is cached, so the question has to be one whose answer
        // is a property of the image (is the .so installed) and not of the
        // environment (is the collector switched on). Flipping CHRONOS_PHP_ENABLED
        // is a runtime decision, and every bridge honours it at runtime — that is
        // where it belongs. Adding or removing the .so changes the image, and a new
        // image compiles a new container.
        if (!NativeExtension::loaded()) {
            return;
        }

        $container
            ->register(ChronosHttpKernel::class, ChronosHttpKernel::class)
            ->setDecoratedService('http_kernel')
            ->setArguments([new Reference(ChronosHttpKernel::class.'.inner')]);

        $container->register(Chronos::class, Chronos::class)->setPublic(true);
        $container->register('chronos', Chronos::class)->setPublic(true);

        $container->addCompilerPass(new ChronosCachePass());
        // Priority 1 (default passes run at 0): ChronosIntegrationsPass writes
        // inputs that FrameworkBundle's MessengerPass and doctrine-bundle's
        // middleware pass consume in the same before-optimization phase, and it
        // must land its Monolog pushHandler before LoggerChannelPass clones the
        // default channel's calls onto the others — see the pass's own doc block.
        $container->addCompilerPass(
            new ChronosIntegrationsPass(),
            PassConfig::TYPE_BEFORE_OPTIMIZATION,
            1,
        );
    }
}
