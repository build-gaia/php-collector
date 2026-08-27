<?php

declare(strict_types=1);

namespace Chronos\Collector\Framework\Symfony;

use Chronos\Collector\Service\CacheCapture;
use Chronos\Collector\Service\SpanManager;
use Psr\Cache\CacheItemInterface;
use Symfony\Component\Cache\Adapter\AdapterInterface;
use Symfony\Contracts\Service\ResetInterface;
use Throwable;

/**
 * Decorates a Symfony cache pool so each get is a child span with hit/miss and,
 * on a hit, the application's own (unserialized) value. Delegates every other
 * AdapterInterface method unchanged. Fail-open: a broken span never affects the
 * cache call it was only meant to observe.
 */
class ChronosCacheAdapter implements AdapterInterface, ResetInterface
{
    public function __construct(
        private readonly AdapterInterface $inner,
        private readonly string $store,
    ) {
    }

    protected function inner(): AdapterInterface
    {
        return $this->inner;
    }

    public function get(string $key, callable $callback, ?float $beta = null, ?array &$metadata = null): mixed
    {
        $missed = false;
        $value = $this->inner->get(
            $key,
            static function (mixed $item) use ($callback, &$missed): mixed {
                $missed = true;

                return $callback($item);
            },
            $beta,
            $metadata,
        );
        $this->recordRead($key, !$missed, $missed ? null : $value);

        return $value;
    }

    public function getItem(string $key): CacheItemInterface
    {
        $item = $this->inner->getItem($key);
        $this->recordItem($item);

        return $item;
    }

    public function getItems(array $keys = []): iterable
    {
        $out = [];
        foreach ($this->inner->getItems($keys) as $key => $item) {
            $this->recordItem($item);
            $out[$key] = $item;
        }

        return $out;
    }

    public function hasItem(string $key): bool
    {
        return $this->inner->hasItem($key);
    }

    public function clear(string $prefix = ''): bool
    {
        return $this->inner->clear($prefix);
    }

    public function deleteItem(string $key): bool
    {
        return $this->inner->deleteItem($key);
    }

    public function deleteItems(array $keys): bool
    {
        return $this->inner->deleteItems($keys);
    }

    public function save(CacheItemInterface $item): bool
    {
        return $this->inner->save($item);
    }

    public function saveDeferred(CacheItemInterface $item): bool
    {
        return $this->inner->saveDeferred($item);
    }

    public function commit(): bool
    {
        return $this->inner->commit();
    }

    public function delete(string $key): bool
    {
        return $this->inner->delete($key);
    }

    public function reset(): void
    {
        if ($this->inner instanceof ResetInterface) {
            $this->inner->reset();
        }
    }

    private function recordItem(CacheItemInterface $item): void
    {
        try {
            $hit = $item->isHit();
            $this->recordRead($item->getKey(), $hit, $hit ? $item->get() : null);
        } catch (Throwable) {
        }
    }

    private function recordRead(string $key, bool $hit, mixed $value): void
    {
        try {
            $span = SpanManager::open($this->store.' GET');
            if (!$span->isVoid()) {
                $span->add('span.kind', 'client');
                $span->add('cache.system', 'cache');
                $span->add('cache.store', $this->store);
                $span->add('db.operation', 'GET');
                $span->add('cache_key', $key);
                CacheCapture::stamp($span, $hit, $value);
            }
            $span->finish();
        } catch (Throwable) {
        }
    }
}
