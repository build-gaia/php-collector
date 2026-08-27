<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony;

use Symfony\Component\Cache\Adapter\AdapterInterface;
use Symfony\Component\Cache\Adapter\TagAwareAdapterInterface;
use Symfony\Component\DependencyInjection\Compiler\CompilerPassInterface;
use Symfony\Component\DependencyInjection\ContainerBuilder;
use Symfony\Component\DependencyInjection\Reference;

/**
 * Wrap every `cache.pool` with ChronosCacheAdapter so Symfony cache reads show
 * hit/miss and the hit value, the same facts Laravel's CacheHit/CacheMissed
 * events already supply.
 */
final class ChronosCachePass implements CompilerPassInterface
{
    public function process(ContainerBuilder $container): void
    {
        if (!interface_exists(AdapterInterface::class)) {
            return;
        }
        foreach ($container->findTaggedServiceIds('cache.pool') as $id => $tags) {
            if ($id === 'cache.system' || str_starts_with($id, 'cache.system.')) {
                continue;
            }
            if (str_starts_with($id, 'chronos.cache.')) {
                continue;
            }
            $store = self::storeName($id, $tags[0] ?? []);
            $decoratorId = 'chronos.cache.'.$id;
            $container
                ->register($decoratorId, self::decoratorClass($container, $id))
                ->setDecoratedService($id)
                ->setArguments([new Reference($decoratorId.'.inner'), $store]);
        }
    }

    /** @param array<string, mixed> $tag */
    private static function storeName(string $id, array $tag): string
    {
        $name = $tag['name'] ?? $tag['namespace'] ?? $id;
        if (!is_string($name) || $name === '') {
            return $id;
        }
        return str_starts_with($name, 'cache.') ? substr($name, 6) : $name;
    }

    private static function decoratorClass(ContainerBuilder $container, string $id): string
    {
        if (!interface_exists(TagAwareAdapterInterface::class)) {
            return ChronosCacheAdapter::class;
        }
        try {
            $class = $container->findDefinition($id)->getClass();
        } catch (\Throwable) {
            return ChronosCacheAdapter::class;
        }
        if (is_string($class) && is_a($class, TagAwareAdapterInterface::class, true)) {
            return ChronosTagAwareCacheAdapter::class;
        }

        return ChronosCacheAdapter::class;
    }
}
