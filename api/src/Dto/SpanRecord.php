<?php

declare(strict_types=1);

namespace Chronos\Collector\Dto;

final class SpanRecord
{
    /** @param array<string, string> $attributes */
    public function __construct(
        public readonly string $traceId,
        public readonly string $spanId,
        public readonly string $parentSpanId,
        public readonly string $name,
        public readonly string $startedAt,
        public readonly string $endedAt,
        public readonly string $status,
        public readonly array $attributes,
    ) {
    }

    /** @return array<string, mixed> */
    public function projection(): array
    {
        return [
            'traceId' => $this->traceId,
            'spanId' => $this->spanId,
            'parentSpanId' => $this->parentSpanId,
            'name' => $this->name,
            'startedAt' => $this->startedAt,
            'endedAt' => $this->endedAt,
            'status' => $this->status,
            'attributes' => $this->attributes,
            'httpMethod' => '',
            'httpRoute' => '',
            'httpStatusCode' => 0,
        ];
    }
}
