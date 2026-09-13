# Chronos PHP Collector

Observability for PHP applications: APM traces, continuous profiling, full HTTP
stack capture, log correlation, and deterministic-simulation recording — collected
by a native extension (`chronos.so`), spooled to a shared volume, and drained
automatically by the Chronos Docker stack's collector.
Designed to be safe to bake into every image: with `CHRONOS_PHP_ENABLED=0`,
both the extension and this package cost your application nothing. Set it to `0`
explicitly rather than leaving it unset — see [Cost guarantees](#cost-guarantees)
for the one thing that is not free by default.

> **`build-gaia/php-collector` is a read-only publishing mirror.** The source
> lives at `build-gaia/collector` under `sdks/php`, and is split out to this
> repository on every push so Packagist finds `composer.json` at a repository
> root. A commit made here is overwritten by the next split — open pull
> requests against `build-gaia/collector` instead.

## Install

### 1. The native extension

Grab the `chronos.so` matching your PHP version and libc from the releases page
(builds exist for PHP 8.1 / 8.2 / 8.4, glibc and musl), or build it yourself with
`make php-extension` in the collector repo. Then, in your `Dockerfile` /
`Dockerfile.dev`:

```dockerfile
# PHP 8.2 (Debian/bookworm images). For Alpine images use the -musl build.
COPY chronos.so /usr/local/lib/php/extensions/no-debug-non-zts-20220829/chronos.so
RUN echo "extension=chronos.so" > /usr/local/etc/php/conf.d/90-chronos.ini
```

Extension-dir names per version: 8.1 → `20210902`, 8.2 → `20220829`,
8.4 → `20240924` (or just install to `$(php-config --extension-dir)`).

The extension alone gives you: a root span per request, automatic curl / PDO /
mysqli / Redis / Memcached client spans with W3C `traceparent` propagation,
headers/cookies/query/body capture (redacted at capture time), the sampling
profiler, and deploy tracking from your `.git`. No PHP code required.

### 2. Configuration

Create a `.chronos` file at your project root (found automatically by walking up
from the document root):

```ini
# Identity: all four are required; without them the collector stays inert.
enabled=1
organisation=org_a2a69137-6d90-49cc-90b9-5d3e49f1ef96
team_id=my-team
application=my-app
# The mounted chronos-spool volume, see step 4. Comments must sit on their own
# line: the parser does not strip a trailing `# ...` from a value, so an inline
# comment would become part of the path.
spool_directory=/var/lib/chronos/spool

# Capture tiers. Rates are fractions: 1 = all, 0.1 = a tenth. All default to off
# EXCEPT the counted profile (`profile_deterministic`), which is on — see below.
apm_enabled=1
apm_sample_rate=1            # trace every request
profiler_enabled=1
profile_request_rate=0       # web requests: profile on demand only
profile_job_rate=0.1         # background jobs: a tenth of them
logs_enabled=1
```

Every setting can equally be set as an environment variable
(`CHRONOS_PHP_ENABLED=1`, `CHRONOS_PHP_ORGANISATION=my-org`, …) or in php.ini
(`chronos.enabled=1`, `chronos.organisation=my-org`, …). Precedence:
**env > php.ini > `.chronos` file**. The `.chronos` file accepts both spellings
(`enabled=1` and `CHRONOS_PHP_ENABLED=1`), so it can double as a dotenv include.

That precedence is the whole security model, not an implementation detail: the
`.chronos` file ships with your application's own code, so anyone who can edit it
can already run code in the process — trusting it grants no new capability. Env
and php.ini, by contrast, are set by whoever controls the platform underneath the
application, and a value pinned at that layer can never be overridden by a file
the application ships. On top of the ordering, `organisation` / `team_id` /
`project` / `application` read from the `.chronos` file are validated as
identifier-shaped (`[A-Za-z0-9._-]`, 1–128 bytes) and `spool_directory` as an
absolute path with no `..` segment; a value that fails validation is treated as
absent (never half-applied) and logged once, by key name only — env and php.ini
values are trusted as given and never validated.

### 3. The PHP package (optional, recommended)

```bash
composer require build-gaia/php-collector
```

The extension works without it; the package adds what only userland can know:

- **Laravel** — zero configuration: the service provider auto-discovers, and you
  get route pattern/name/action/middleware on the request span, SQL spans with
  connection identity and bound parameters, cache hit/miss/write/forget spans
  with the hit value, log→trace correlation, exact response capture, bounded
  `framework.views` / `.models` / `.mail` / `.authorization` counts, authenticated
  user id and peak memory, plus the `messaging.events` / `messaging.jobs`
  catalogs.

  Spans, not just counts, for the work that takes the time: **template renders**
  (one span per view, nested the way `@include` nests them), **transaction
  boundaries** (the queries inside nest under the `BEGIN`, and a transaction the
  request abandoned is closed and marked rather than dropped), **Redis commands**
  the cache layer did not issue — the facade, locks, sessions, the rate limiter —
  and **framework bootstrap**, measured from `LARAVEL_START` to the container's
  `booted` callback. Every exception the application *reports* becomes a span too,
  including the ones it recovered from and never showed the user; the request root
  carries only the one that reached the response.

  **Queued jobs are one trace, end to end.** The W3C context is stamped into the
  job payload at dispatch (through Laravel's own `createPayloadUsing` seam, so it
  survives retries, releases and any queue driver), and the worker opens a
  job-scoped request against it — so the job's queries, HTTP calls and failures
  hang beneath the request that queued it. This needs no `CHRONOS_PHP_CLI_ENABLED`:
  that flag gates only the extension's AUTOMATIC start in RINIT, and the bridge
  starts each job's request explicitly. Leave it off — with it on, the worker
  PROCESS gets a request of its own and the first job of each worker inherits
  everything since boot. What the worker does need is this package in its vendor
  tree; the extension alone carries no queue instrumentation.

  The dispatch instant rides in the payload beside the context, so the job root
  also carries `messaging.message.queue_time_ms`: **how long the message waited**,
  which is the one number that separates a backed-up queue from a slow job — they
  produce identical job durations. It is dispatch-to-start (a deliberately delayed
  job counts its delay as wait; the intent is on the dispatching request as its
  `messaging.jobs` record's `delay_ms`), and it is absent rather than zero when
  the wait cannot be proven — an older payload with no stamp, or a dispatcher
  whose clock runs ahead of the worker's.

  The catalogs are the part worth reading twice. An event or a queued job is
  recorded with its **destination, transport, encoding and dispatch call site**,
  not just its name — and a message that actually leaves the process (a broadcast
  event, a job on any driver but `sync`) additionally gets its own producer span
  carrying `messaging.system` / `messaging.destination.name` /
  `messaging.operation`, which is what draws the edge to the broker on the service
  map. See ADR 0024 for the vocabulary and its bounds.
- **bunny/bunny (raw AMQP)** — `BunnyTelemetry` joins a publish and a consume into
  one trace where there is no framework seam to hook: no envelope, no stamp, no
  event, just a channel method and a delivery callback. Two call sites change.
  `$channel->publish($body, $headers, $exchange, $routingKey)` becomes
  `BunnyTelemetry::publish($channel, $body, $headers, $exchange, $routingKey,
  vhost: $vhost, messageName: $class, contentType: 'application/x-protobuf')` —
  the first six arguments are Bunny's own signature verbatim, and the trailing
  ones are the facts Bunny cannot supply (its `$options` is protected with no
  getter, so the vhost is unreachable from a `Channel`). And
  `$channel->consume($callback, $queue)` becomes
  `$channel->consume(BunnyTelemetry::consumer($callback, $queue, $vhost), $queue)`
  — wrapped at the callback, the OUTERMOST point, so the ack and the payload
  decode happen inside the traced request.

  The publish injects `traceparent` (a child, so the consumer hangs beneath the
  publish rather than claiming to be it), `tracestate`, `baggage` and an
  `x-chronos-enqueued-at` stamp into the AMQP application headers, and records a
  producer span; each delivery becomes its own `QUEUE`-rooted request carrying
  `messaging.message.queue_time_ms`, `messaging.message.redelivered` and the
  normalised destination. A publish to a named topic exchange carries NO
  `messaging.destination.name`: a topic has no queue, and which queues are bound
  to that routing key is not knowable from the publisher — such a span is named
  after the exchange instead, which is the same string the consumer's binding
  names. Like the Laravel queue bridge this needs no `CHRONOS_PHP_CLI_ENABLED`,
  and leaving it off is better for the same reason. `bunny/bunny` stays a
  composer `suggest`: the bridge only declares for an application that already
  has it.
- **Symfony** — register `Chronos\Collector\Framework\Symfony\ChronosBundle`
  (or decorate the kernel with `ChronosHttpKernel`). Cache pools are wrapped so
  reads show hit/miss and the unserialized hit value.
- **symfony1** — register `Chronos\Collector\Framework\Symfony1\ChronosFilter`
  in `filters.yml`.
- **Custom spans** — `$chronos->span->create('name')`, or declare an
  instrumentation manifest of `Chronos\trace_method('Class', 'method')` calls and
  point `instrumentation_manifest` at it — no application code changes.
- **Inline query plans** — `EXPLAIN` capture on the real statement with the real
  binds (`CHRONOS_PHP_EXPLAIN=1`).

### 4. Connect to the Chronos stack

There is nothing to configure. The local Chronos Docker stack owns a shared
volume named `chronos-spool` and runs a collector that automatically drains
every file written into it — the spool file itself carries the
org/project/application identity, so one collector serves every service on the
machine. Your app just mounts the volume where `spool_directory` points:

```yaml
services:
  my-app:
    volumes:
      - "chronos-spool:/var/lib/chronos/spool"

volumes:
  chronos-spool:
    external: true   # created and drained by the Chronos stack
```

Write telemetry to the path; it appears in Chronos. No ingest URL, token, or
certificate ever enters your application's configuration.

(Deploying an app somewhere the stack's volume can't reach — another host, a
remote environment? Run your own `chronos-engine-agent` next to the app pointed
at its spool directory; see `engine/deploy/php-spool-forwarder.md`.)

## Cost guarantees

- **Extension installed, `enabled` explicitly `0`**: requests start nothing, the
  Zend observer is never registered, the package's framework hooks register
  nothing. One cached config check per process. Measured on PHP 8.3: a
  function-call-bound workload runs at the same speed as with no extension
  installed at all.
- **Extension installed, `enabled` UNSET**: everything above holds except the
  observer, which IS registered — and costs roughly **30 ns on every userland
  function call** whether or not anything is being collected (measured ~19 ns/call
  without the extension, ~50 ns/call with it). `zend_observer_fcall_register` is
  MINIT-only, so the decision cannot be deferred to the first request; an absent
  setting has to assume a later request might turn collection on. Write the `0`
  and the cost goes away. On a request making 100k calls the difference is ~3 ms;
  at 1M calls, ~30 ms.
- **Package installed, extension absent**: every call is
  `extension_loaded`/`function_exists`-guarded and fail-open — silent no-ops,
  nothing registered.
- **Enabled, request not head-sampled** (`apm_sample_rate` < 1): the sample
  decision is made in Rust before any capture; unsampled requests skip HTTP
  capture and response-body copies entirely.
- **Enabled and sampled, but not profile-sampled** (`profile_request_rate` < 1):
  no timer is armed and no stacks are walked. Profiling has its own rate — it
  used to ride the APM decision, so profiling a percent of traffic meant
  discarding most of your traces with it.
- **Counted profile (`profile_deterministic`, on by default)**: this is the one
  tier that does work on an unsampled request, so it is worth being precise. It
  adds no new pass over your code — the Zend observer already ran begin/end
  trampolines on every userland call and discarded the result. Enabling it also
  removes two heap allocations the observer previously paid *per call* (the
  function name is now interned per `zend_function` instead of being rebuilt
  every time), and adds a clock-read pair and one hashmap upsert. The net
  per-call effect is a design intent, **not a benchmarked guarantee** — measure
  it on your workload before relying on a number. Memory is bounded by DISTINCT
  functions (`profile_deterministic_max_functions`), never by call volume, and
  internal builtins are still untouched. `profile_deterministic=0` opts out.
- **CLI / workers**: not auto-collected unless `cli_enabled=1`; a queue worker
  integration can still start/end request scopes explicitly.

## Settings reference

Every setting has one canonical env name. The other two spellings derive from it
mechanically: strip the `CHRONOS_PHP_` / `CHRONOS_` prefix and lowercase for the
`.chronos` key, prefix with `chronos.` for php.ini. So
`CHRONOS_PHP_APM_ENABLED` is `apm_enabled` in a `.chronos` file and
`chronos.apm_enabled` in php.ini. Precedence: **env > php.ini > `.chronos`**.

This table is COMPLETE — every name in the collector's `settings::SETTING_NAMES`
appears below, and `scripts/verify-collector-settings-docs.sh` fails the build if
one is added without a row here.

### Identity — required; without these the collector stays inert

| `.chronos` key | env | default |
|---|---|---|
| `enabled` | `CHRONOS_PHP_ENABLED` | `0` — the master switch |
| `organisation` | `CHRONOS_PHP_ORGANISATION` | — (required) |
| `team_id` | `CHRONOS_PHP_TEAM_ID` | — the team owning this service; overrides `project` |
| `project` | `CHRONOS_PHP_PROJECT` | — (required, unless `team_id` is set) |
| `application` | `CHRONOS_PHP_APPLICATION` | — (required) |
| `spool_directory` | `CHRONOS_PHP_SPOOL_DIRECTORY` | — (required) |

A team and a project are the same thing under two names. `team_id` is the
current spelling: a service declares its owning team by writing it into its own
`.chronos`, and the estate allocates the service to that team on sight. Setting
both names is not a conflict — `team_id` is taken and `project` ignored.

### Deploy identity — resolved from `.git` when unset

| `.chronos` key | env | default |
|---|---|---|
| `app_version` | `CHRONOS_APP_VERSION` | nearest git tag |
| `app_commit` | `CHRONOS_APP_COMMIT` | git HEAD |
| `app_branch` | `CHRONOS_APP_BRANCH` | git branch |
| `app_language_version` | `CHRONOS_PHP_APP_LANGUAGE_VERSION` | `PHP_VERSION`, reported by the SDK bridge |
| `app_framework` | `CHRONOS_PHP_APP_FRAMEWORK` | reported by the SDK bridge |
| `app_framework_version` | `CHRONOS_PHP_APP_FRAMEWORK_VERSION` | reported by the SDK bridge |

### Capture tiers

| `.chronos` key | env | default |
|---|---|---|
| `apm_enabled` | `CHRONOS_PHP_APM_ENABLED` | `0` |
| `logs_enabled` | `CHRONOS_PHP_LOGS_ENABLED` | `0` |
| `profiler_enabled` | `CHRONOS_PHP_PROFILER_ENABLED` | `0` |
| `cli_enabled` | `CHRONOS_PHP_CLI_ENABLED` | `0` — CLI/workers are not auto-collected |
| `dst_enabled` | `CHRONOS_PHP_DST_ENABLED` | `0` — lab/CLI only; ignored when `env` is `production`/`prod`, where the only path is the `x-chronos-dst` header / `chronos_dst` cookie |
| `env` | `CHRONOS_PHP_ENV` | — (`production`/`prod` refuses process-wide DST) |

There is no runtime-metrics tier. The extension used to write one `.metrics`
spool file per request behind `CHRONOS_PHP_RUNTIME_METRICS_ENABLED`, carrying a
request count, its duration and (on Linux) `VmRSS`. Every one of those numbers
is already on the request's root span, which the engine now reads for traffic,
latency and served routes — so the tier bought a second `fsync()` on the
request's own critical path and nothing else. The three tiers above are the
whole surface.

### Sample rates

**All rates are FRACTIONS**: `1` is all of it, `0.1` a tenth, `0` none. They are
quantised to `sample_rate_denominator`, so at the default of 1000 the finest rate
you can express is `0.001` — one in a thousand. A rate that rounds to zero is
reported on the collector's startup line rather than silently sampling nothing.

The older spelling was basis points (`apm_sample_rate=10000` for 100%). Since a
fraction can never exceed 1, **any value above 1 is still read as basis points**,
so existing files keep working and the startup line names which settings were
read that way. The one ambiguous value is exactly `1`: basis points meant 0.01%,
a fraction means 100%, and it is read as the fraction.

| `.chronos` key | env | default |
|---|---|---|
| `apm_sample_rate` | `CHRONOS_PHP_APM_SAMPLE_RATE` | `1` — trace every locally-rooted request |
| `profile_request_rate` | `CHRONOS_PHP_PROFILE_REQUEST_RATE` | `0` — web requests are not profiled unless asked |
| `profile_job_rate` | `CHRONOS_PHP_PROFILE_JOB_RATE` | `0.1` — a tenth of background jobs |
| `sample_rate_denominator` | `CHRONOS_PHP_SAMPLE_RATE_DENOMINATOR` | `1000` (clamped to 10..1,000,000) |
| `profile_token` | `CHRONOS_PHP_PROFILE_TOKEN` | — empty disables the forced-profile directive entirely |

An inbound `traceparent` always wins: only a locally-rooted trace makes its own
APM decision, so `apm_sample_rate` does not apply to a request that arrived with
one. A *forced* profile is the single exception — it is an explicit human
instruction, so it keeps its trace even against the caller's decision.

### Profiling on demand

With `profile_token` set, one request is profiled whatever the rate says:

```bash
curl -H "X-Chronos-Profile: $TOKEN" https://your-service/checkout
```

…or a whole browser session, via a `chronos_profile=$TOKEN` cookie. The token is
mandatory and compared in constant time: a header that makes the server do
materially more work must never be armable by an unauthenticated caller.

A forced profile also forces its trace to be kept — a profile is reached
*through* its request, so one without a sampled trace is an orphan. Profiles
carry `trigger=forced` or `trigger=sampled`, and job profiles carry
`workload=job`, so one deliberate capture is never mistaken for a representative
sample and the two populations can be separated when aggregating.

### Which requests count as background jobs

A request is a job when its method is **not** one of the nine HTTP verbs — the
Laravel queue bridge reports `QUEUE`, a native CLI start reports nothing at all.
It is a positive list of web verbs on purpose: a future `CRON`/`CONSUME` bridge
lands on the job rate automatically, whereas a list of job words would silently
fall through to the web rate of zero and profile nothing.

The job rate only bites where jobs are ALREADY traced: native CLI needs
`cli_enabled=1`, and a framework worker has to open a job through the SDK bridge.
A service that never instrumented its workers profiles nothing.

### Profiler shape

| `.chronos` key | env | default |
|---|---|---|
| `profile_sample_rate` | `CHRONOS_PHP_PROFILE_SAMPLE_RATE` | `99` **Hz** — the stack-walk frequency, NOT a fraction of requests. `0` disables the sampler |
| `profile_types` | `CHRONOS_PHP_PROFILE_TYPES` | all four: `cpu`, `wall`, `off_cpu`, `io` |
| `profile_series_id` | `CHRONOS_PHP_PROFILE_SERIES_ID` | `php` |
| `profile_io_min_us` | `CHRONOS_PHP_PROFILE_IO_MIN_US` | `1000` — I/O waits shorter than this are not worth a stack walk |

`profile_sample_rate` (how densely a profiled request is sampled) and
`profile_request_rate` (how many requests get profiled) are easy to confuse. The
names are close because the second one arrived later; read the units.

### Counted profile (deterministic)

Exact per-function call counts and inclusive/exclusive time, from the Zend
observer rather than the sampler. Three tiers, and only the first is on by
default.

| `.chronos` key | env | default |
|---|---|---|
| `profile_deterministic` | `CHRONOS_PHP_PROFILE_DETERMINISTIC` | `1` — **on by default**, unlike every other capture tier. See the cost note below |
| `profile_deterministic_max_functions` | `CHRONOS_PHP_PROFILE_DETERMINISTIC_MAX_FUNCTIONS` | `4096` distinct functions per request |
| `profile_edges` | `CHRONOS_PHP_PROFILE_EDGES` | `0` — tier 2, caller/callee edges |
| `profile_edges_max` | `CHRONOS_PHP_PROFILE_EDGES_MAX` | `8192` distinct edges per request |
| `profile_args` | `CHRONOS_PHP_PROFILE_ARGS` | `0` — tier 3, argument capture. Needs a forced profile or armed DST *as well as* this flag |
| `profile_args_max_args` | `CHRONOS_PHP_PROFILE_ARGS_MAX_ARGS` | `8` arguments per call |
| `profile_args_max_arg_bytes` | `CHRONOS_PHP_PROFILE_ARGS_MAX_ARG_BYTES` | `256` bytes per argument |
| `profile_args_max_total_bytes` | `CHRONOS_PHP_PROFILE_ARGS_MAX_TOTAL_BYTES` | `4096` bytes per request |
| `profile_args_max_invocations` | `CHRONOS_PHP_PROFILE_ARGS_MAX_INVOCATIONS` | `4` retained invocations per function |

**Tier 1 — counts and timing (default on).** Every observed userland function
gets a call count, an inclusive time and an exclusive time. Recursion is banked
once on the outermost frame, so a recursive function's inclusive time is not
multiplied by its depth. Internal builtins (`strpos`, `array_map`, …) are NOT
counted — they carry no observer trampoline, and attaching one to them is where
the real cost would be.

**Tier 2 — edges (opt in).** Caller-to-callee totals, which is what makes
per-call-path attribution possible. Without it, a tier 1 row sums *every* path
through a function.

**Tier 3 — arguments (opt in, and gated twice).** Bounded, redacted, scalar-only
argument values, and ONLY for functions the instrumentation manifest
allowlisted via `Chronos\trace_method`. It stays inert unless `profile_args=1`
**and** the request carries a forced-profile directive or DST recording is armed.
Composite values (arrays, objects, resources) are never captured — only their
type is recorded. Redaction uses the same key patterns as `redact_patterns`.
This is deliberately not a general parameter recorder: capturing every argument
on every call would make the collector a PII firehose, which is why it is
allowlist-scoped rather than rate-scoped.

Every cap **reports rather than silently drops**: when a request exceeds one, the
spooled document carries the seen/kept counts, so a truncated profile is visibly
truncated instead of quietly wrong.

Numbers from this tier are exact, but they are scoped to the FUNCTION, not to one
call path — a figure shown against a flame-graph frame covers every path through
that function, and reads higher than the frame's own subtree. Tier 2 is what
narrows it to a path.

### Span detail

| `.chronos` key | env | default |
|---|---|---|
| `span_all_userland` | `CHRONOS_PHP_SPAN_ALL_USERLAND` | `0` |
| `span_min_duration_us` | `CHRONOS_PHP_SPAN_MIN_DURATION_US` | `100` |
| `exclude_paths` | `CHRONOS_PHP_EXCLUDE_PATHS` | `/vendor/,/node_modules/,/cache/,/.git/` |
| `instrumentation_manifest` | `CHRONOS_PHP_INSTRUMENTATION_MANIFEST` | — |
| `local_rich_telemetry` | `CHRONOS_PHP_LOCAL_RICH_TELEMETRY` | `0` |

### HTTP capture

| `.chronos` key | env | default |
|---|---|---|
| `http_capture` | `CHRONOS_PHP_HTTP_CAPTURE` | `1` |
| `http_capture_bodies` | `CHRONOS_PHP_HTTP_CAPTURE_BODIES` | `1` |
| `http_capture_max_body` | `CHRONOS_PHP_HTTP_CAPTURE_MAX_BODY` | `65536` (64 KiB) |
| `http_capture_response_buffer` | `CHRONOS_PHP_HTTP_CAPTURE_RESPONSE_BUFFER` | `0` |
| `http_capture_redact` | `CHRONOS_PHP_HTTP_CAPTURE_REDACT` | `1` |
| `redact_patterns` | `CHRONOS_PHP_REDACT_PATTERNS` | `authorization`, `password`, `credential`, `private_key`, `client_secret`, `access_token`, `refresh_token`, `secret`, `api_key` — matched case-insensitively against the KEY |

### Messaging capture

| `.chronos` key | env | default |
|---|---|---|
| `messaging_capture_bodies` | `CHRONOS_PHP_MESSAGING_CAPTURE_BODIES` | `0` |
| `messaging_capture_max_body` | `CHRONOS_PHP_MESSAGING_CAPTURE_MAX_BODY` | `65536` (64 KiB) |

Note the default: this is OFF where `http_capture_bodies` is ON. Installing an APM
agent is already a decision to look at your own request and response bodies; an
inter-service message payload is a different one — it was written by one team for
another team's consumer, and capturing it copies that contract into telemetry a
third audience reads.

**No field-level masking applies to a body on either path.** `redact_patterns`
masks map ENTRIES — header and query-parameter keys — and a body is only ever
truncated. A protobuf payload has no field names on the wire to match against at
all. So this flag is the whole control: with it on, complete payloads ship, PII
included.

The configured size is a ceiling, not the effective limit. A publish body rides a
span attribute capped at 16 KiB (12 KiB of raw bytes once base64 inflates a binary
payload); a consume body rides the request-attribute bag, capped at 8 KiB (6 KiB
raw). The SDK truncates to whichever is smaller and sets
`messaging.message.body.truncated`, so a cut body always says it was cut.
Non-UTF-8 payloads — which is every protobuf — are base64-encoded and marked with
`messaging.message.body.encoding`, because the value has to cross into a Rust
`String` and then through `serde_json`, and both require valid UTF-8.

### DST and spool

| `.chronos` key | env | default |
|---|---|---|
| `dst_call_path_max` | `CHRONOS_PHP_DST_CALL_PATH_MAX` | `4096` — DST-gated first-party call visits |
| `dst_call_path_max_depth` | `CHRONOS_PHP_DST_CALL_PATH_MAX_DEPTH` | `64` |
| `spool_max_bytes` | `CHRONOS_PHP_SPOOL_MAX_BYTES` | `921600` (900 KiB) per spooled document |

### Query plans (PHP package only)

These live in the Composer package rather than the extension, so they are read
from the PHP environment (`getenv`, `$_ENV`, `$_SERVER`) and — when the extension
is loaded — from a `.chronos` file. **Not from php.ini**: an unregistered
`chronos.*` INI directive cannot be set, and these are not in the extension's
registration list. Both name spellings are accepted; `LOCAL_` is the family the
other development-only switches use.

| env (either spelling) | default |
|---|---|
| `CHRONOS_PHP_EXPLAIN` / `CHRONOS_PHP_LOCAL_EXPLAIN` | off — `EXPLAIN` the real statement with its real binds |
| `CHRONOS_PHP_EXPLAIN_WRITES` / `CHRONOS_PHP_LOCAL_EXPLAIN_WRITES` | off — also explain `INSERT`/`UPDATE`/`DELETE`/`REPLACE` |
| `CHRONOS_PHP_EXPLAIN_MAX_PER_REQUEST` / `CHRONOS_PHP_LOCAL_EXPLAIN_MAX_PER_REQUEST` | `5` (clamped to 1..50) |

Reading a setting from PHP: `chronos_setting('apm_sample_rate')` accepts any name
in the tables above, resolved through the same precedence chain.

## Development

```bash
php api/tests/verify.php   # package test suite (spec-conformance sections
                           # auto-skip outside the platform monorepo)
```
