<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony;

use Symfony\Component\Cache\Adapter\TagAwareAdapterInterface;

final class ChronosTagAwareCacheAdapter extends ChronosCacheAdapter implements TagAwareAdapterInterface
{
    public function invalidateTags(array $tags): bool
    {
        $inner = $this->inner();

        return $inner instanceof TagAwareAdapterInterface
            ? $inner->invalidateTags($tags)
            : false;
    }
}
