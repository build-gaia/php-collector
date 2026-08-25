<?php

declare(strict_types=1);

namespace Chronos\Collector\Replay;

/**
 * The Chronos Replay Protocol v1 engine: discovery, lookup resolution, effect policy, and the
 * divergence report (protocol §3, §6, §7, §8).
 *
 * This class is the whole protocol and none of the interception. It never touches a clock, a
 * driver or a socket — a caller hands it `(channel, selector, intent)` and it hands back the
 * recorded answer or a loud refusal. That split is deliberate: everything above this line is
 * language-specific and belongs to the runtime, everything in here is the contract another
 * language's runtime has to reproduce byte for byte to replay the same recording.
 *
 * The governing bias, stated once so every branch below can be read against it: a replay that
 * cannot answer a call FAILS LOUDLY. A replay that quietly substitutes a plausible value
 * destroys the only thing replay is for, which is telling the operator that this code, on
 * these inputs, no longer behaves as recorded.
 */
final class ReplaySession
{
    public const STRICT_FULL = 'full';
    public const STRICT_ANSWERS = 'answers';

    public const OUTCOME_CONFORMANT = 'conformant';
    public const OUTCOME_DIVERGED = 'diverged';
    public const OUTCOME_ABORTED = 'aborted';
    public const OUTCOME_PRECONDITION_FAILED = 'precondition_failed';

    /**
     * Exit codes sit between the shell's own range and Docker's reserved 125–127, and clear of
     * 128+signal, so a diverged replay can never be mistaken for a container-level failure.
     */
    public const EXIT_CONFORMANT = 0;
    public const EXIT_DIVERGED = 90;
    public const EXIT_PRECONDITION_FAILED = 91;
    public const EXIT_ABORTED = 92;

    private ?Recording $recording = null;

    private ?EffectPolicy $policy = null;

    private string $protocol = Recording::PROTOCOL_VERSION;

    private string $strict = self::STRICT_FULL;

    private string $reportPath = Report::DEFAULT_PATH;

    /** @var array<string, int> per-(channel, selector) cursor */
    private array $cursors = [];

    /** @var array<int, bool> recorded sequences this replay has consumed */
    private array $consumed = [];

    /** @var list<Answer> */
    private array $lookups = [];

    /** @var list<Divergence> */
    private array $divergences = [];

    /** @var array<string, bool> channels whose `simulated` mode was downgraded to `replayed` */
    private array $downgraded = [];

    private int $step = 0;

    private bool $aborted = false;

    private bool $finished = false;

    private string $outcome = self::OUTCOME_CONFORMANT;

    private int $exitCode = self::EXIT_CONFORMANT;

    private readonly string $startedAt;

    private string $finishedAt = '';

    /** @var array<string, string> */
    private array $environment;

    /** @param array<string, string> $environment */
    private function __construct(
        public readonly string $recordingId,
        array $environment,
    ) {
        $this->environment = $environment;
        $this->startedAt = self::now();
        $report = $environment['CHRONOS_REPLAY_REPORT'] ?? '';
        if ($report !== '') {
            $this->reportPath = $report;
        }
        // Anything other than the one documented relaxation means `full`. Strictness is not an
        // effect mode: getting it wrong costs a severity, not a wrong answer, so refusing to
        // run over a typo here would trade a real verdict for a pedantic one.
        if (($environment['CHRONOS_REPLAY_STRICT'] ?? '') === self::STRICT_ANSWERS) {
            $this->strict = self::STRICT_ANSWERS;
        }
        try {
            $this->discover($environment);
        } catch (PreconditionFailed $failure) {
            $this->failPrecondition($failure);
        }
    }

    /**
     * Construct a session from the sandbox environment, or return null when this process is
     * not a replay.
     *
     * The absence of CHRONOS_REPLAY_RECORDING is what keeps ONE image usable for both
     * recording and replay: with no recording id the runtime must behave exactly as an
     * ordinary run, must not read the recorded input tree, and must not write a report.
     *
     * A precondition failure does not throw out of here. The session comes back already
     * reported and already carrying exit code 91, because a caller that has to catch an
     * exception to find out the plan was wrong will one day forget to.
     *
     * @param array<string, string> $environment
     */
    public static function begin(array $environment): ?self
    {
        $recordingId = trim((string) ($environment['CHRONOS_REPLAY_RECORDING'] ?? ''));
        if ($recordingId === '') {
            return null;
        }

        return new self($recordingId, $environment);
    }

    public function isRunning(): bool
    {
        return $this->recording !== null && !$this->aborted && !$this->finished;
    }

    public function outcome(): string
    {
        return $this->outcome;
    }

    public function exitCode(): int
    {
        return $this->exitCode;
    }

    public function strict(): string
    {
        return $this->strict;
    }

    public function reportPath(): string
    {
        return $this->reportPath;
    }

    /**
     * Resolve one lookup (protocol §6.4).
     *
     * $live performs the real effect and is consulted ONLY under `passthrough`. It returns the
     * live answer's payload, or throws to say the effect could not be performed. Passing null
     * under `passthrough` means the same thing as throwing: the runtime has no way to reach the
     * dependency the plan asked it to reach.
     */
    public function resolve(Lookup $lookup, ?callable $live = null): Answer
    {
        if ($this->recording === null || $this->policy === null) {
            throw new \LogicException('replay session did not start: '.$this->outcome);
        }
        if ($this->aborted || $this->finished) {
            throw new \LogicException('replay session is over: '.$this->outcome);
        }
        $step = ++$this->step;
        $mode = $this->modeFor($lookup);
        $cursor = $lookup->channel."\0".$lookup->selector;
        $ordinal = $this->cursors[$cursor] ?? 0;
        // Advanced on a miss and on a block as well as on a hit, so a repeated failing call
        // reports ordinals 0, 1, 2 instead of three identical findings at ordinal 0.
        $this->cursors[$cursor] = $ordinal + 1;

        if ($mode === EffectPolicy::BLOCKED) {
            return $this->blocked($step, $lookup, $ordinal);
        }
        $matches = $this->recording->matches($lookup->channel, $lookup->selector);
        $event = $matches[$ordinal] ?? null;
        if ($mode === EffectPolicy::PASSTHROUGH) {
            return $this->passthrough($step, $lookup, $ordinal, $event, $live);
        }
        if ($event === null) {
            return $this->miss($step, $lookup, $ordinal, $mode, $matches === []);
        }

        return $this->hit($step, $lookup, $ordinal, $mode, $event);
    }

    /**
     * Settle the replay: detect unconsumed events, decide the outcome, write the report.
     * Idempotent, because it is reached both from the ordinary end of a request and from a
     * shutdown handler that cannot know whether the ordinary path already ran.
     */
    public function finish(): int
    {
        if ($this->finished) {
            return $this->exitCode;
        }
        $this->finished = true;
        $unconsumed = 0;
        if ($this->recording !== null && !$this->aborted) {
            $unconsumed = $this->reportUnconsumed();
        }
        $this->settle();
        $this->finishedAt = self::now();
        Report::write($this->report($unconsumed), $this->reportPath);

        return $this->exitCode;
    }

    /**
     * The report as it stands. Exposed so a runtime can log or forward it; `finish()` is what
     * writes it.
     *
     * @return array<string, mixed>
     */
    public function report(int $unconsumed = 0): array
    {
        $consumptions = [];
        foreach ($this->lookups as $answer) {
            if ($answer->sequence === null) {
                continue;
            }
            $entry = $answer->toArray();
            unset($entry['outcome'], $entry['value']);
            $consumptions[] = $entry;
        }

        return [
            'schema' => Report::SCHEMA,
            'protocol' => $this->protocol,
            'recordingId' => $this->recordingId,
            'runtime' => [
                'name' => 'php',
                'version' => PHP_VERSION,
                'implementation' => ReplayRuntime::IMPLEMENTATION,
            ],
            'startedAt' => $this->startedAt,
            'finishedAt' => $this->finishedAt !== '' ? $this->finishedAt : self::now(),
            'outcome' => $this->outcome,
            'exitCode' => $this->exitCode,
            'strict' => $this->strict,
            'counts' => $this->counts($unconsumed),
            'channels' => $this->channels(),
            'consumptions' => $consumptions,
            // Not in the protocol's own schema, and added on purpose: `consumptions` cannot
            // describe a lookup that consumed nothing, so a blocked write or an aborted miss
            // would be visible only as a divergence. A consumer that does not know the field
            // ignores it (protocol §9).
            'lookups' => array_map(static fn (Answer $answer): array => $answer->toArray(), $this->lookups),
            'divergences' => array_map(
                static fn (Divergence $divergence): array => $divergence->toArray(),
                $this->ordered(),
            ),
            // ADR 0021 Phase 3 — optional path-diff location. Protocol §9 ignores unknown fields.
            ...(($callPath = $this->callPathFragment()) !== null ? ['callPath' => $callPath] : []),
        ];
    }

    /**
     * @return array{
     *     recordedCount: int,
     *     executedCount: int,
     *     firstDivergence: array{index: int, recorded: ?array{name: string, depth: int}, executed: ?array{name: string, depth: int}}|null
     * }|null
     */
    private function callPathFragment(): ?array
    {
        $recorded = $this->recording !== null
            ? CallPath::fromEvents($this->recording->events())
            : [];
        $executedFile = trim((string) ($this->environment['CHRONOS_REPLAY_CALL_PATH_EXECUTED'] ?? ''));
        $executed = $executedFile !== ''
            ? CallPath::fromFile($executedFile)
            : CallPath::executed();

        return CallPath::reportFragment($recorded, $executed);
    }

    /**
     * Discovery and integrity (protocol §3), then the two policy checks that have to happen
     * before the first lookup.
     *
     * A guessed inputs path is refused rather than defaulted. The planner's mount point is
     * configuration — it is /recorded/dst today and the sandbox only guarantees it lives under
     * /recorded/ — so a runtime that guessed would one day mount-mismatch and replay a
     * DIFFERENT recording than the plan intended, which is the worst failure this protocol can
     * have.
     *
     * @param array<string, string> $environment
     *
     * @throws PreconditionFailed
     */
    private function discover(array $environment): void
    {
        $inputs = trim((string) ($environment['CHRONOS_REPLAY_INPUTS'] ?? ''));
        if ($inputs === '') {
            throw new PreconditionFailed(
                Divergence::RECORDING_UNAVAILABLE,
                'CHRONOS_REPLAY_RECORDING is set but CHRONOS_REPLAY_INPUTS is not; refusing to guess a mount path',
            );
        }
        $recording = Recording::load($inputs, $this->recordingId);
        // Reported as the version actually used, so a version disagreement is visible after the
        // fact rather than inferred from strange results.
        $this->protocol = $recording->requireSupportedVersion((string) ($environment['CHRONOS_REPLAY_PROTOCOL'] ?? ''));
        $policy = EffectPolicy::fromEnvironment($environment);
        $this->recording = $recording;
        $this->policy = $policy;

        if ($recording->truncated()) {
            $this->raise(new Divergence(
                step: 0,
                type: Divergence::RECORDING_TRUNCATED,
                severity: Divergence::severityOf(Divergence::RECORDING_TRUNCATED, $this->strict),
                message: 'the recorder hit its event ceiling: this recording is a prefix of the request, so a late miss may be the ceiling rather than the code',
            ));
        }
        $this->downgradeUnsimulatableChannels();
    }

    /**
     * `simulated` is refused for `time` and `random` and downgraded to `replayed`
     * (protocol §7.4).
     *
     * A synthetic clock reading or RNG value is indistinguishable from a real one, so
     * simulating either would silently de-determinise the replay — the exact outcome the
     * protocol exists to prevent. The downgrade is announced at discovery rather than at the
     * first clock read because it is a fact about the PLAN, and it is worth telling the
     * operator even if the code never reads the clock at all.
     */
    private function downgradeUnsimulatableChannels(): void
    {
        foreach ([Vocabulary::CHANNEL_TIME, Vocabulary::CHANNEL_RANDOM] as $channel) {
            if ($this->policy?->modeFor($channel, Vocabulary::INTENT_READ) !== EffectPolicy::SIMULATED) {
                continue;
            }
            $this->downgraded[$channel] = true;
            $this->raise(new Divergence(
                step: 0,
                type: Divergence::SIMULATED_SUBSTITUTION,
                severity: Divergence::severityOf(Divergence::SIMULATED_SUBSTITUTION, $this->strict),
                channel: $channel,
                mode: EffectPolicy::SIMULATED,
                message: sprintf(
                    'channel %s cannot be simulated: a synthetic value here is indistinguishable from a real one, so the mode is downgraded to %s',
                    $channel,
                    EffectPolicy::REPLAYED,
                ),
            ));
        }
    }

    private function modeFor(Lookup $lookup): string
    {
        $mode = $this->policy?->modeFor($lookup->channel, $lookup->intent) ?? EffectPolicy::BLOCKED;

        return $mode === EffectPolicy::SIMULATED && isset($this->downgraded[$lookup->channel])
            ? EffectPolicy::REPLAYED
            : $mode;
    }

    /**
     * `blocked`: no effect, and no lookup either — not even when a recorded answer exists.
     *
     * Mode outranks the recording. The plan said this effect class is off, and serving it from
     * the recording would make `blocked` mean `replayed`. The caller is handed no value at all;
     * the layer above turns this into the language's natural error for a refused operation, and
     * does not catch it on the application's behalf.
     */
    private function blocked(int $step, Lookup $lookup, int $ordinal): Answer
    {
        $this->raise(new Divergence(
            step: $step,
            type: Divergence::EFFECT_BLOCKED,
            severity: Divergence::severityOf(Divergence::EFFECT_BLOCKED, $this->strict),
            channel: $lookup->channel,
            selector: $lookup->selector,
            intent: $lookup->intent,
            ordinal: $ordinal,
            mode: EffectPolicy::BLOCKED,
            site: $lookup->site,
            message: sprintf('%s %s refused by replay effect policy', $lookup->channel, $lookup->intent),
        ));

        return $this->record(new Answer(
            step: $step,
            outcome: Answer::BLOCKED,
            payload: null,
            sequence: null,
            answerSequence: null,
            eventDigest: null,
            payloadDigest: null,
            redaction: null,
            channel: $lookup->channel,
            selector: $lookup->selector,
            intent: $lookup->intent,
            ordinal: $ordinal,
            mode: EffectPolicy::BLOCKED,
            fatal: false,
            divergence: Divergence::EFFECT_BLOCKED,
        ));
    }

    private function hit(int $step, Lookup $lookup, int $ordinal, string $mode, RecordedEvent $event): Answer
    {
        $answerEvent = $this->recording?->answerFor($event);
        $source = $answerEvent ?? $event;
        $this->consume($event, $answerEvent);
        $answer = $this->record(new Answer(
            step: $step,
            outcome: Answer::HIT,
            payload: $source->payload,
            sequence: $event->sequence,
            answerSequence: $answerEvent?->sequence,
            // The digest of the event whose payload is being handed back, so the report's
            // identity and the report's value always describe the same bytes.
            eventDigest: $source->eventDigest,
            payloadDigest: $source->payloadDigest,
            redaction: $source->redaction,
            channel: $lookup->channel,
            selector: $lookup->selector,
            intent: $lookup->intent,
            ordinal: $ordinal,
            mode: $mode,
            fatal: false,
        ));
        $this->compareExpectation($step, $lookup, $answer);

        return $answer;
    }

    /**
     * A miss under `replayed` aborts; a miss under `simulated` substitutes.
     *
     * Under `replayed` there is no honest answer available: the recorded run never made this
     * call, so no value exists that is both plausible and true. Not a zero, not an empty result
     * set, not a fresh clock reading, not a live fetch.
     *
     * `unrecorded_effect` and `ordinal_exhausted` stay apart because they are different bugs.
     * The first says the code now does something NEW; the second says it does the same thing
     * MORE OFTEN. Collapsing them would cost the reader the distinction most likely to point at
     * the change.
     */
    private function miss(int $step, Lookup $lookup, int $ordinal, string $mode, bool $absent): Answer
    {
        if ($mode === EffectPolicy::SIMULATED) {
            return $this->simulate($step, $lookup, $ordinal);
        }
        $type = $absent ? Divergence::UNRECORDED_EFFECT : Divergence::ORDINAL_EXHAUSTED;
        $this->raise(new Divergence(
            step: $step,
            type: $type,
            severity: Divergence::severityOf($type, $this->strict),
            channel: $lookup->channel,
            selector: $lookup->selector,
            intent: $lookup->intent,
            ordinal: $ordinal,
            mode: $mode,
            site: $lookup->site,
            message: $absent
                ? sprintf('the recording contains no %s call for this selector', $lookup->channel)
                : sprintf('the recording contains %d such %s calls; this is call %d', $ordinal, $lookup->channel, $ordinal + 1),
        ));
        $this->aborted = true;

        return $this->record($this->missAnswer($step, $lookup, $ordinal, $mode, $type));
    }

    private function simulate(int $step, Lookup $lookup, int $ordinal): Answer
    {
        $synthetic = Vocabulary::syntheticAnswer($lookup->channel, $lookup->intent, $ordinal);
        $this->raise(new Divergence(
            step: $step,
            type: Divergence::SIMULATED_SUBSTITUTION,
            severity: Divergence::severityOf(Divergence::SIMULATED_SUBSTITUTION, $this->strict),
            channel: $lookup->channel,
            selector: $lookup->selector,
            intent: $lookup->intent,
            ordinal: $ordinal,
            mode: EffectPolicy::SIMULATED,
            observed: $synthetic,
            site: $lookup->site,
            message: 'the recording had no answer; a declared synthetic answer was substituted',
        ));

        return $this->record(new Answer(
            step: $step,
            outcome: Answer::SIMULATED,
            payload: $synthetic,
            sequence: null,
            answerSequence: null,
            eventDigest: null,
            payloadDigest: null,
            redaction: null,
            channel: $lookup->channel,
            selector: $lookup->selector,
            intent: $lookup->intent,
            ordinal: $ordinal,
            mode: EffectPolicy::SIMULATED,
            fatal: false,
            divergence: Divergence::SIMULATED_SUBSTITUTION,
        ));
    }

    /**
     * `passthrough` performs the real effect and returns the LIVE value (protocol §7.5).
     *
     * A mismatch against the recording is `divergent` and not fatal, because seeing the
     * difference was the whole point of asking for live behaviour. An effect that cannot be
     * performed aborts and never falls back to the recording: falling back would answer a
     * comparison that never happened, and the operator would draw conclusions from it.
     */
    private function passthrough(
        int $step,
        Lookup $lookup,
        int $ordinal,
        ?RecordedEvent $event,
        ?callable $live,
    ): Answer {
        $performed = null;
        $failure = $live === null ? 'no live effect performer is available in this runtime' : null;
        if ($live !== null) {
            try {
                $result = $live();
                if (!is_array($result)) {
                    $failure = 'the live effect performer returned no payload';
                } else {
                    $performed = $result;
                }
            } catch (\Throwable $error) {
                $failure = $error->getMessage();
            }
        }
        if ($performed === null) {
            $this->raise(new Divergence(
                step: $step,
                type: Divergence::EFFECT_UNAVAILABLE,
                severity: Divergence::severityOf(Divergence::EFFECT_UNAVAILABLE, $this->strict),
                channel: $lookup->channel,
                selector: $lookup->selector,
                intent: $lookup->intent,
                ordinal: $ordinal,
                mode: EffectPolicy::PASSTHROUGH,
                sequence: $event?->sequence,
                expected: $event === null ? null : ['sequence' => $event->sequence, 'eventDigest' => $event->eventDigest],
                site: $lookup->site,
                message: 'passthrough was required and the effect could not be performed: '.((string) $failure),
            ));
            $this->aborted = true;

            return $this->record($this->missAnswer(
                $step,
                $lookup,
                $ordinal,
                EffectPolicy::PASSTHROUGH,
                Divergence::EFFECT_UNAVAILABLE,
            ));
        }
        $answerEvent = $event === null ? null : $this->recording?->answerFor($event);
        $source = $answerEvent ?? $event;
        $liveDigest = Canonical::eventDigest($source?->kind ?? '', $performed);
        if ($event !== null) {
            $this->consume($event, $answerEvent);
        }
        if ($source !== null && $liveDigest !== $source->eventDigest) {
            $this->raise(new Divergence(
                step: $step,
                type: Divergence::PASSTHROUGH_MISMATCH,
                severity: Divergence::severityOf(Divergence::PASSTHROUGH_MISMATCH, $this->strict),
                channel: $lookup->channel,
                selector: $lookup->selector,
                intent: $lookup->intent,
                ordinal: $ordinal,
                mode: EffectPolicy::PASSTHROUGH,
                sequence: $source->sequence,
                expected: ['sequence' => $source->sequence, 'eventDigest' => $source->eventDigest],
                observed: ['eventDigest' => $liveDigest],
                site: $lookup->site,
                message: 'the live answer differs from the recorded one',
            ));
        } elseif ($source === null) {
            // Nothing is wrong, but a replay that reached outside the sandbox is always worth
            // recording.
            $this->raise(new Divergence(
                step: $step,
                type: Divergence::PASSTHROUGH_PERFORMED,
                severity: Divergence::severityOf(Divergence::PASSTHROUGH_PERFORMED, $this->strict),
                channel: $lookup->channel,
                selector: $lookup->selector,
                intent: $lookup->intent,
                ordinal: $ordinal,
                mode: EffectPolicy::PASSTHROUGH,
                observed: ['eventDigest' => $liveDigest],
                site: $lookup->site,
                message: 'a live effect was performed with no recorded counterpart',
            ));
        }
        $answer = $this->record(new Answer(
            step: $step,
            outcome: Answer::PASSTHROUGH,
            payload: $performed,
            sequence: $event?->sequence,
            answerSequence: $answerEvent?->sequence,
            eventDigest: $liveDigest,
            payloadDigest: null,
            redaction: null,
            channel: $lookup->channel,
            selector: $lookup->selector,
            intent: $lookup->intent,
            ordinal: $ordinal,
            mode: EffectPolicy::PASSTHROUGH,
            fatal: false,
        ));
        $this->compareExpectation($step, $lookup, $answer);

        return $answer;
    }

    private function missAnswer(int $step, Lookup $lookup, int $ordinal, string $mode, string $type): Answer
    {
        return new Answer(
            step: $step,
            outcome: Answer::MISS,
            payload: null,
            sequence: null,
            answerSequence: null,
            eventDigest: null,
            payloadDigest: null,
            redaction: null,
            channel: $lookup->channel,
            selector: $lookup->selector,
            intent: $lookup->intent,
            ordinal: $ordinal,
            mode: $mode,
            fatal: true,
            divergence: $type,
        );
    }

    /**
     * Compare a caller's expectation against the answer, key by key (protocol §6.5).
     *
     * Keys the answer does not carry are not compared, so a partial expectation is safe. The
     * expectation never changes what is returned and never affects selection: the recorded
     * answer is still the answer, and this only turns the calling code's belief about it into a
     * reported contradiction.
     */
    private function compareExpectation(int $step, Lookup $lookup, Answer $answer): void
    {
        if ($lookup->expectation === null || $answer->payload === null) {
            return;
        }
        foreach ($lookup->expectation as $key => $expected) {
            $key = (string) $key;
            if (!array_key_exists($key, $answer->payload)) {
                continue;
            }
            $recorded = Canonical::text($answer->payload[$key]);
            if ($recorded === Canonical::text($expected)) {
                continue;
            }
            $this->raise(new Divergence(
                step: $step,
                type: Divergence::VALUE_MISMATCH,
                severity: Divergence::severityOf(Divergence::VALUE_MISMATCH, $this->strict),
                channel: $lookup->channel,
                selector: $lookup->selector,
                intent: $lookup->intent,
                ordinal: $answer->ordinal,
                mode: $answer->mode,
                sequence: $answer->sequence,
                expected: ['key' => $key, 'value' => $recorded],
                observed: ['key' => $key, 'value' => Canonical::text($expected)],
                site: $lookup->site,
                message: sprintf('the recorded answer contradicts the caller on %s', $key),
            ));
        }
    }

    /**
     * Every recorded request-or-self-answering event this replay never consumed
     * (protocol §8.4).
     *
     * An aborted run reports none: the replay stopped early, so everything past the abort point
     * is trivially unconsumed and listing it would bury the one finding that matters.
     * `counts.unconsumed` is then 0 and it means "not assessed", not "none".
     */
    private function reportUnconsumed(): int
    {
        $count = 0;
        foreach ($this->recording?->selectable() ?? [] as $event) {
            if (isset($this->consumed[$event->sequence])) {
                continue;
            }
            ++$count;
            $this->raise(new Divergence(
                step: 0,
                type: Divergence::UNCONSUMED_EVENT,
                severity: Divergence::severityOf(Divergence::UNCONSUMED_EVENT, $this->strict),
                channel: $event->channel,
                selector: $event->selector,
                sequence: $event->sequence,
                expected: ['sequence' => $event->sequence, 'eventDigest' => $event->eventDigest],
                message: 'the recording carries this effect and the replayed code never performed it',
            ));
        }

        return $count;
    }

    /**
     * An answer event consumed alongside its request is not tracked separately: it was never
     * selectable, so reporting it unconsumed would invent a finding.
     */
    private function consume(RecordedEvent $event, ?RecordedEvent $answer): void
    {
        $this->consumed[$event->sequence] = true;
        if ($answer !== null) {
            $this->consumed[$answer->sequence] = true;
        }
    }

    private function record(Answer $answer): Answer
    {
        $this->lookups[] = $answer;

        return $answer;
    }

    private function raise(Divergence $divergence): void
    {
        $this->divergences[] = $divergence;
    }

    private function failPrecondition(PreconditionFailed $failure): void
    {
        $this->raise(new Divergence(
            step: 0,
            type: $failure->type,
            severity: Divergence::severityOf($failure->type, $this->strict),
            message: $failure->getMessage(),
        ));
        $this->outcome = self::OUTCOME_PRECONDITION_FAILED;
        $this->exitCode = self::EXIT_PRECONDITION_FAILED;
        $this->finished = true;
        $this->finishedAt = self::now();
        // Written here rather than from finish(): the workload never runs, so nothing else is
        // going to get the chance to write it.
        Report::write($this->report(), $this->reportPath);
    }

    /**
     * The run's outcome is the worst severity reached (protocol §8.2). The distinction between
     * 91 and 92 is who the reader should look at: 91 means no verdict was reached — the plan,
     * the mount or the policy was wrong and nothing was learned about the code — while 92 means
     * the verdict IS the finding.
     */
    private function settle(): void
    {
        if ($this->outcome === self::OUTCOME_PRECONDITION_FAILED) {
            return;
        }
        if ($this->aborted) {
            $this->outcome = self::OUTCOME_ABORTED;
            $this->exitCode = self::EXIT_ABORTED;

            return;
        }
        foreach ($this->divergences as $divergence) {
            if ($divergence->severity === Divergence::DIVERGENT || $divergence->severity === Divergence::FATAL) {
                $this->outcome = self::OUTCOME_DIVERGED;
                $this->exitCode = self::EXIT_DIVERGED;

                return;
            }
        }
        $this->outcome = self::OUTCOME_CONFORMANT;
        $this->exitCode = self::EXIT_CONFORMANT;
    }

    /**
     * Ordered by step, then by the order raised. Entries raised outside a lookup carry step 0
     * and sort AFTER every lookup-raised entry, so discovery-time and finish-time findings end
     * up adjacent instead of scattered.
     *
     * @return list<Divergence>
     */
    private function ordered(): array
    {
        $ordered = $this->divergences;
        usort($ordered, static fn (Divergence $left, Divergence $right): int
            => ($left->step === 0 ? PHP_INT_MAX : $left->step) <=> ($right->step === 0 ? PHP_INT_MAX : $right->step));

        return $ordered;
    }

    /** @return array<string, int> */
    private function counts(int $unconsumed): array
    {
        $counts = [
            'lookups' => count($this->lookups),
            'hits' => 0,
            'misses' => 0,
            'blocked' => 0,
            'simulated' => 0,
            'passthrough' => 0,
            'unconsumed' => $unconsumed,
        ];
        foreach ($this->lookups as $answer) {
            switch ($answer->outcome) {
                case Answer::HIT:
                    ++$counts['hits'];
                    break;
                case Answer::BLOCKED:
                    ++$counts['blocked'];
                    break;
                case Answer::PASSTHROUGH:
                    // Not a miss either way: under passthrough the recording is not the source
                    // of the answer, only of the comparison.
                    ++$counts['passthrough'];
                    break;
                case Answer::SIMULATED:
                    ++$counts['simulated'];
                    ++$counts['misses'];
                    break;
                default:
                    ++$counts['misses'];
            }
        }

        return $counts;
    }

    /** @return list<array<string, mixed>> */
    private function channels(): array
    {
        $recorded = [];
        $consumed = [];
        foreach ($this->recording?->selectable() ?? [] as $event) {
            $recorded[$event->channel] = ($recorded[$event->channel] ?? 0) + 1;
            $consumed[$event->channel] = ($consumed[$event->channel] ?? 0)
                + (isset($this->consumed[$event->sequence]) ? 1 : 0);
        }
        ksort($recorded);
        $channels = [];
        foreach ($recorded as $channel => $total) {
            $channels[] = [
                'channel' => $channel,
                'recorded' => $total,
                'consumed' => $consumed[$channel] ?? 0,
                // Zero on an aborted run, matching counts.unconsumed: not assessed.
                'unconsumed' => $this->aborted ? 0 : $total - ($consumed[$channel] ?? 0),
            ];
        }

        return $channels;
    }

    private static function now(): string
    {
        return (new \DateTimeImmutable('now', new \DateTimeZone('UTC')))->format('Y-m-d\TH:i:s.u\Z');
    }
}
