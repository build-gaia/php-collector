<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony;

use Chronos\Collector\Chronos;
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
    }
}
