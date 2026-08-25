<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * A materialised recording: the manifest, the event stream, the request/answer bindings and
 * the per-(channel, selector) match index — all resolved once, at load, over an immutable
 * input tree (protocol §3).
 *
 * Nothing here is ever written back. The recorded tree is bind-mounted read-only and shared,
 * so a runtime that wrote cursor state, a lock file or a rewritten manifest next to the
 * recording would both fail the sandbox and let one replay observe another's progress.
 */
final class Recording
{
    public const PROTOCOL_VERSION = '1.0';

    /** The highest protocol major this implementation can interpret. */
    public const SUPPORTED_MAJOR = 1;

    private const MANIFEST_FILE = 'manifest.json';
    private const EVENTS_FILE = 'events.json';

    /** @var list<RecordedEvent> */
    private array $events;

    /** @var array<int, RecordedEvent> answer event keyed by the sequence of its request */
    private array $answers;

    /** @var list<RecordedEvent> requests and self-answering events, ascending by sequence */
    private array $selectable;

    /** @var array<string, list<RecordedEvent>> selectable events keyed by "channel\0selector" */
    private array $matches;

    /**
     * @param array<string, mixed> $manifest
     * @param list<RecordedEvent>  $events
     */
    private function __construct(
        public readonly array $manifest,
        public readonly string $protocolVersion,
        array $events,
    ) {
        $this->events = $events;
        [$this->answers, $this->selectable] = self::bind($events);
        $this->matches = [];
        foreach ($this->selectable as $event) {
            $this->matches[self::key($event->channel, $event->selector)][] = $event;
        }
    }

    /**
     * Load and validate the recording mounted at $inputs, which the planner set
     * CHRONOS_REPLAY_INPUTS to.
     *
     * The identity check is not paranoia about the planner: a recording swapped for another
     * one replays the wrong inputs under the right plan, and every downstream conclusion is
     * then false with no visible error — the worst failure this protocol can have. The count
     * check catches a truncated materialisation, which a crash between the planner's two file
     * writes would otherwise leave mounted.
     *
     * @throws PreconditionFailed
     */
    public static function load(string $inputs, string $recordingId): self
    {
        $manifest = self::readJson($inputs.'/'.self::MANIFEST_FILE);
        $events = self::readJson($inputs.'/'.self::EVENTS_FILE);
        if (($manifest['recordingId'] ?? null) !== $recordingId) {
            throw new PreconditionFailed(
                Divergence::RECORDING_UNAVAILABLE,
                sprintf(
                    'recording identity mismatch: mount carries %s, plan asked for %s',
                    var_export($manifest['recordingId'] ?? null, true),
                    $recordingId,
                ),
            );
        }
        $raw = $events['events'] ?? null;
        if (!is_array($raw)) {
            throw new PreconditionFailed(
                Divergence::RECORDING_UNAVAILABLE,
                self::EVENTS_FILE.' carries no events array',
            );
        }
        $declared = $manifest['eventCount'] ?? null;
        if (!is_numeric($declared) || (int) $declared !== count($raw)) {
            throw new PreconditionFailed(
                Divergence::RECORDING_UNAVAILABLE,
                sprintf(
                    'manifest declares %s events, %d are materialised',
                    var_export($declared, true),
                    count($raw),
                ),
            );
        }
        $decoded = [];
        foreach ($raw as $entry) {
            if (is_array($entry)) {
                $decoded[] = RecordedEvent::fromArray($entry);
            }
        }
        // Re-sorted defensively rather than refused. The planner already sorts, and ordering
        // is recoverable, so rejecting an out-of-order stream would lose a whole recording
        // for a cosmetic reason (protocol §3.4).
        usort($decoded, static fn (RecordedEvent $left, RecordedEvent $right): int
            => $left->sequence <=> $right->sequence);

        return new self($manifest, self::resolveVersion($manifest), $decoded);
    }

    /**
     * Resolve the protocol version: the manifest wins over the environment, because the
     * recording is the artefact being interpreted and a sandbox offering 1.4 cannot make a
     * 1.0 recording mean something else.
     *
     * @param array<string, mixed> $manifest
     */
    private static function resolveVersion(array $manifest): string
    {
        $declared = $manifest['protocolVersion'] ?? null;

        return is_string($declared) && $declared !== '' ? $declared : '';
    }

    /**
     * Check the recording's protocol major against this implementation (protocol §4).
     *
     * A higher major is refused outright and never partially read: a major bump exists
     * precisely because the old reading of an existing field would now be wrong, so a
     * best-effort read would produce confident nonsense. A higher MINOR proceeds untouched —
     * within a major, every addition has to be ignorable.
     *
     * @throws PreconditionFailed
     */
    public function requireSupportedVersion(string $offered): string
    {
        $version = $this->protocolVersion !== '' ? $this->protocolVersion
            : ($offered !== '' ? $offered : self::PROTOCOL_VERSION);
        $major = (int) explode('.', $version)[0];
        if ($major > self::SUPPORTED_MAJOR || $major < 1) {
            throw new PreconditionFailed(
                Divergence::PROTOCOL_UNSUPPORTED,
                sprintf('recording declares protocol %s, this runtime implements major %d', $version, self::SUPPORTED_MAJOR),
            );
        }

        return $version;
    }

    /**
     * Whether the recorder hit its own event ceiling, so the recording is a PREFIX of the
     * request rather than the whole of it. Worth announcing once: a late miss may then be the
     * ceiling talking rather than the code.
     */
    public function truncated(): bool
    {
        return ($this->manifest['truncated'] ?? false) === true;
    }

    public function recordingId(): string
    {
        $id = $this->manifest['recordingId'] ?? '';

        return is_string($id) ? $id : '';
    }

    /** @return list<RecordedEvent> */
    public function events(): array
    {
        return $this->events;
    }

    /** @return list<RecordedEvent> */
    public function selectable(): array
    {
        return $this->selectable;
    }

    /** The answer event bound to a request event, or null when it stands alone. */
    public function answerFor(RecordedEvent $event): ?RecordedEvent
    {
        return $this->answers[$event->sequence] ?? null;
    }

    /**
     * The recorded events of $channel whose derived selector equals $selector, ascending by
     * sequence. This is the whole of selection: no adjacency, no global index, nothing that a
     * change in the replayed code's control flow can shift.
     *
     * @return list<RecordedEvent>
     */
    public function matches(string $channel, string $selector): array
    {
        return $this->matches[self::key($channel, $selector)] ?? [];
    }

    /**
     * Resolve request/answer pairing once, over the immutable stream (protocol §6.1).
     *
     * Sequence adjacency is trustworthy HERE and nowhere else: the recording was produced by
     * a run that really did issue that query and really did get that result back. Adjacency
     * between the recording and a divergent replay is not trustworthy, which is why nothing
     * else in this protocol uses sequence for selection. An answer that binds to no request
     * stands alone as a self-answering event of its channel, so it stays both selectable and
     * reportable as unconsumed.
     *
     * @param list<RecordedEvent> $events
     *
     * @return array{array<int, RecordedEvent>, list<RecordedEvent>}
     */
    private static function bind(array $events): array
    {
        $answers = [];
        $bound = [];
        $total = count($events);
        foreach ($events as $index => $event) {
            $answerKind = Vocabulary::answerKindFor($event->kind);
            if ($answerKind === null) {
                continue;
            }
            for ($ahead = $index + 1; $ahead < $total; ++$ahead) {
                $candidate = $events[$ahead];
                if ($candidate->kind === $answerKind && !isset($bound[$candidate->sequence])) {
                    $answers[$event->sequence] = $candidate;
                    $bound[$candidate->sequence] = true;
                    break;
                }
            }
        }
        $selectable = [];
        foreach ($events as $event) {
            if (isset($bound[$event->sequence]) || Vocabulary::isObservational($event->kind)) {
                continue;
            }
            $selectable[] = $event;
        }

        return [$answers, $selectable];
    }

    /**
     * Read one file of the recording. Every failure collapses to the same
     * `recording_unavailable`, because the operator's next action is identical whichever it
     * was: look at the mount, not at the code.
     *
     * @return array<string, mixed>
     *
     * @throws PreconditionFailed
     */
    private static function readJson(string $path): array
    {
        $contents = @file_get_contents($path);
        if ($contents === false) {
            throw new PreconditionFailed(
                Divergence::RECORDING_UNAVAILABLE,
                sprintf('%s is missing or unreadable', $path),
            );
        }
        $decoded = json_decode($contents, true);
        if (!is_array($decoded) && json_last_error() === JSON_ERROR_UTF8) {
            // The recorders cap payload values by byte-slicing without a boundary check, so a
            // long value can end in a broken UTF-8 sequence. That is a recorder defect, not a
            // reason to lose the recording: drop the malformed bytes and read the rest
            // (protocol §5.1).
            $decoded = json_decode($contents, true, 512, JSON_INVALID_UTF8_IGNORE);
        }
        if (!is_array($decoded)) {
            throw new PreconditionFailed(
                Divergence::RECORDING_UNAVAILABLE,
                sprintf('%s is not a JSON object: %s', $path, json_last_error_msg()),
            );
        }

        return $decoded;
    }

    private static function key(string $channel, string $selector): string
    {
        return $channel."\0".$selector;
    }
}
