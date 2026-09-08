<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony;

use Chronos\Collector\Chronos;
use Symfony\Component\DependencyInjection\Compiler\PassConfig;
use Symfony\Component\DependencyInjection\ContainerBuilder;
use Symfony\Component\DependencyInjection\Reference;
use Symfony\Component\HttpKernel\Bundle\Bundle;

final class ChronosBundle extends Bundle
{
    public function build(ContainerBuilder $container): void
    {
        parent::build($container);

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
