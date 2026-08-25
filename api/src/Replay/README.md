# Chronos Replay Protocol — PHP reference implementation

The first implementation of the contract in `docs/specs/replay-chronos-replay-protocol.md`
(v1.0). It conforms against every vector and case in
`docs/specs/replay-protocol-conformance/`; `composer test` runs them.

Nothing in here is a PHP feature. The protocol engine takes an environment array and answers
`(channel, selector, intent)` lookups from a materialised recording; it never touches a clock, a
driver or a socket. Everything language-specific lives above it, in exactly two places: `Effect`
(the call surface) and `ReplayRuntime` (process wiring).

## The files

| File | Role |
| --- | --- |
| `Canonical.php` | canonical text form, the recorder's 4096-byte cap, selector normalisation, `eventDigest` (§5) |
| `Vocabulary.php` | kind→channel table, request/answer pairs, selector derivation, intent, synthetic answers (§6.1–6.3, §7.4) |
| `RecordedEvent.php` | one recorded event with its derived facts |
| `Recording.php` | discovery, integrity checks, load-time pairing, the per-(channel, selector) index (§3, §6.1) |
| `EffectPolicy.php` | which environment variable governs which lookup (§7.2) |
| `Lookup.php` / `Answer.php` / `Divergence.php` | the three value objects the engine speaks in |
| `ReplaySession.php` | the engine: resolution, effect policy, divergence, outcome, exit code (§6.4, §7, §8) |
| `Report.php` | writes the report, with the stderr fallback (§9) |
| `ReplayRuntime.php` | process bootstrap, shutdown handler, exit-code override |
| `Effect.php` | what application code and interception layers call |
| `CallPath.php` | extract `call` visits from a recording; first-divergence for ADR 0021 Phase 3 |
| `MutationSweep.php` | bounded recording mutations for ADR 0021 Phase 4 agent sweeps |
| `bootstrap.php` | `auto_prepend_file` entry point for a replay image |
| `ReplayBlocked.php` / `ReplayAborted.php` | the two ways a lookup refuses to produce a value |

## The three decisions worth knowing before reading the code

**Selection is keyed, never sequenced.** A lookup resolves against
`(channel, selector)` with a per-key ordinal. Recorded `sequence` is used for exactly one thing:
binding `database_query`→`database_result` and `http_request`→`http_response`, once, at load, over
the immutable recording. Nothing during replay uses adjacency, because a replay exists to run
*changed* code and one inserted call would shift every later index — silently.

**A miss is loud.** Under `replayed` an unanswerable lookup writes the report and exits 92. There
is no zero value, no empty result set, no fresh clock read, no live fetch. `simulated` is the only
mode allowed to invent, and it marks every invention in both the value and the report.

**`blocked` outranks the recording.** No lookup is performed even when a recorded answer exists,
and the caller gets a `ReplayBlocked` exception rather than a value. The runtime does not catch it
on the application's behalf.

## Implementing the same contract in another language

The suite is data-only so no runtime is privileged. A Node, Bun or Go implementation is the same
six pieces, and the order matters — the vectors catch mistakes the cases can only report as
confusing diffs:

1. **`Canonical`** — byte-oriented, no locale, no ini dependency. Get `digest-vectors.json` green
   before anything else. The two traps are that a nested string inside a JSON value stays quoted
   (`{"a":"1"}`, not `{"a":1}`) and that the 4096-byte cap drops a trailing partial UTF-8 sequence
   rather than splitting it.
2. **`Vocabulary`** — pure tables. `selector-vectors.json` covers them. Cap *before* collapsing
   whitespace: the recorder capped the raw value, so collapsing first keeps more of a long
   statement than the recording holds and then never matches it.
3. **`Recording`** — load, verify `recordingId` and `eventCount`, sort by sequence, bind pairs
   once, index by `(channel, selector)`. Never write to the input tree.
4. **`EffectPolicy`** — three named variables win over the generic
   `CHRONOS_REPLAY_EFFECT_<CHANNEL>_READ`/`_WRITE` pair; validate every effect variable eagerly.
5. **`ReplaySession`** — per-key cursors that advance on misses and blocks too, the four modes,
   the closed divergence set, unconsumed detection at finish, the outcome/exit-code table.
6. **The runtime layer** — whatever that language uses to intercept its own clock, driver and
   HTTP client, routed through one call surface shaped like `Effect`. Return the recorded answer,
   throw on `blocked`, and end the process on a fatal.

Two things a port should NOT copy: the `auto_prepend_file` bootstrap (a Node runtime uses a
loader hook or `--require`), and `ReplayRuntime`'s shutdown handler (a language with a real
`process.exit` and no fatal-error unwinding does not need one). Both are PHP working around PHP.

The residual portability limit is stated in the specification and holds here: `time` and `random`
fall back to a recorded symbol name, so a PHP recording's `mt_rand` events mean nothing to a Go
replay. Channels with real selectors — `database`, `http`, `cache`, `env`, `file` — are
cross-runtime.

## Interception: what exists and what is still needed

`Effect` is the surface. It is called by userland adapters (a PDO wrapper, a Guzzle middleware, a
Laravel queue decorator) and returns `null` when the process is not a replay, so an adapter that
calls it is safe in production.

What userland **cannot** do is intercept PHP's internal functions. The native extension's Zend
observer sees a call start and end but cannot replace a return value or suppress the real call,
which is exactly what replay needs. The concrete asks against the native crate, in priority order:

1. **An internal-function handler override table**, armed only when `CHRONOS_REPLAY_RECORDING` is
   set, that can (a) short-circuit the real call and (b) substitute a return value. The existing
   observer allowlist cannot do either — this is a `zend_function.internal_function.handler`
   swap, in the shape ddtrace uses, not an observer hook.
2. **A userland callback seam** on that table: `chronos_replay_delegate(callable)`, invoked as
   `fn(string $kind, array $inputs): ?array`, so the answer keeps coming from this one
   implementation of the protocol rather than a second copy of it in Rust. If the callback returns
   null the real function runs; if it throws, the throw propagates.
3. The functions worth arming first, with the channel and selector each maps to:

   | Native symbols | Channel | Selector |
   | --- | --- | --- |
   | `time`, `microtime`, `hrtime`, `date`, `gettimeofday` | `time` | the symbol |
   | `mt_rand`, `rand`, `random_int`, `random_bytes`, `uniqid` | `random` | the symbol |
   | `getenv` | `env` | the variable name |
   | `PDO::query`, `PDO::exec`, `PDOStatement::execute`, `mysqli_query` | `database` | the statement |
   | `curl_exec`, `curl_multi_exec` | `http` | `METHOD url` from the handle's options |
   | `file_get_contents`, `fopen`, `fwrite` | `file` | the path |

   `PDO`/`PDOStatement` is the hard one and the most valuable: substituting a result set means
   returning a synthetic statement object, not a scalar. Until it exists, a Laravel application is
   better served by a userland PDO wrapper bound in the container.

4. Redis and the queue need no native hook: `Framework/Predis/ChronosPredisClient` and Laravel's
   queue connector are already userland seams that can call `Effect::cache()` and
   `Effect::queue()`.

Nothing in this list changes the protocol. Every one of these is a way of getting a call to
`Effect`; the contract underneath is finished.
