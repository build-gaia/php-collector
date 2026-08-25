# Chronos PHP Collector

Observability for PHP applications: APM traces, continuous profiling, full HTTP
stack capture, log correlation, and deterministic-simulation recording — collected
by a native extension (`chronos.so`), spooled to disk, and shipped by a forwarder.
Designed to be safe to bake into every image: with `CHRONOS_PHP_ENABLED` unset,
both the extension and this package cost your application nothing.

## Install

### 1. The native extension

Grab the `chronos.so` matching your PHP version and libc from the releases page
(builds exist for PHP 8.1 / 8.2 / 8.4, glibc and musl), or build it yourself with
`make php-extension` in the collector repo. Then, in your `Dockerfile` /
`Dockerfile.dev`:

```dockerfile
# PHP 8.2 (Debian/bookworm images). For Alpine images use the -musl build.
COPY chronos.so /usr/local/lib/php/extensions/no-debug-non-zts-20220829/chronos.so
RUN echo "extension=chronos.so" > /usr/local/etc/php/conf.d/90-chronos.ini \
 && mkdir -p /var/lib/chronos/php && chown www-data /var/lib/chronos/php
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
organisation=my-org
project=my-project
application=my-app
spool_directory=/var/lib/chronos/php

# Capture tiers — each defaults to off.
apm_enabled=1
apm_sample_rate=10000        # basis points: 10000 = 100%
profiler_enabled=1
logs_enabled=1
```

Every setting can equally be set as an environment variable
(`CHRONOS_PHP_ENABLED=1`, `CHRONOS_PHP_ORGANISATION=my-org`, …) or in php.ini
(`chronos.enabled=1`, `chronos.organisation=my-org`, …). Precedence:
**env > php.ini > `.chronos` file**. The `.chronos` file accepts both spellings
(`enabled=1` and `CHRONOS_PHP_ENABLED=1`), so it can double as a dotenv include.

### 3. The PHP package (optional, recommended)

```bash
composer require build-gaia/php-collector
```

The extension works without it; the package adds what only userland can know:

- **Laravel** — zero configuration: the service provider auto-discovers, and you
  get route patterns, SQL spans with connection identity and bound parameters,
  cache hit/miss spans, log→trace correlation, and exact response capture.
- **Symfony** — decorate your kernel with
  `Chronos\Collector\Framework\Symfony\ChronosHttpKernel`.
- **symfony1** — register `Chronos\Collector\Framework\Symfony1\ChronosFilter`
  in `filters.yml`.
- **Custom spans** — `$chronos->span->create('name')`, or declare an
  instrumentation manifest of `Chronos\trace_method('Class', 'method')` calls and
  point `instrumentation_manifest` at it — no application code changes.
- **Inline query plans** — `EXPLAIN` capture on the real statement with the real
  binds (`CHRONOS_PHP_EXPLAIN=1`).

### 4. Shipping

Telemetry lands as files in `spool_directory`; run the `chronos-engine-agent`
forwarder as a sidecar (or host daemon) pointing at the same directory. See
`engine/deploy/php-spool-forwarder.md` in the platform repository.

## Cost guarantees

- **Extension installed, `enabled` unset/0**: requests start nothing, observers
  emit nothing, the package's framework hooks register nothing. One cached
  config check per process.
- **Package installed, extension absent**: every call is
  `extension_loaded`/`function_exists`-guarded and fail-open — silent no-ops,
  nothing registered.
- **Enabled, request not head-sampled** (`apm_sample_rate` < 10000): the sample
  decision is made in Rust before any capture; unsampled requests skip HTTP
  capture, profiling, and response-body copies entirely.
- **CLI / workers**: not auto-collected unless `cli_enabled=1`; a queue worker
  integration can still start/end request scopes explicitly.

## Settings reference

| `.chronos` / php.ini (`chronos.*`) | env | default |
|---|---|---|
| `enabled` | `CHRONOS_PHP_ENABLED` | `0` (master switch) |
| `organisation` / `project` / `application` | `CHRONOS_PHP_ORGANISATION` / `_PROJECT` / `_APPLICATION` | — (required) |
| `spool_directory` | `CHRONOS_PHP_SPOOL_DIRECTORY` | — (required) |
| `apm_enabled` | `CHRONOS_PHP_APM_ENABLED` | `0` |
| `apm_sample_rate` | `CHRONOS_PHP_APM_SAMPLE_RATE` | `10000` bps |
| `logs_enabled` | `CHRONOS_PHP_LOGS_ENABLED` | `0` |
| `profiler_enabled` | `CHRONOS_PHP_PROFILER_ENABLED` | `0` |
| `profile_sample_rate` | `CHRONOS_PHP_PROFILE_SAMPLE_RATE` | `99` Hz |
| `dst_enabled` | `CHRONOS_PHP_DST_ENABLED` | `0` (lab/CLI only; ignored when `CHRONOS_PHP_ENV=production` — use `x-chronos-dst` / `chronos_dst`) |
| `env` | `CHRONOS_PHP_ENV` | — (`production`/`prod` refuses process-wide DST) |
| `runtime_metrics_enabled` | `CHRONOS_PHP_RUNTIME_METRICS_ENABLED` | `0` |
| `cli_enabled` | `CHRONOS_PHP_CLI_ENABLED` | `0` |
| `http_capture` / `http_capture_bodies` | `CHRONOS_PHP_HTTP_CAPTURE` / `_BODIES` | `1` |
| `instrumentation_manifest` | `CHRONOS_PHP_INSTRUMENTATION_MANIFEST` | — |
| `app_version` | `CHRONOS_APP_VERSION` | resolved from `.git` |

Full list: `chronos_setting()` accepts any name from the collector's
`settings::SETTING_NAMES`.

## Development

```bash
php api/tests/verify.php   # package test suite (spec-conformance sections
                           # auto-skip outside the platform monorepo)
```
