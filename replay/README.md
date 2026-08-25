# Chronos replay image (PHP)

Builds an image that carries an application's code plus the Chronos replay shim armed in replay
mode, for `CHRONOS_SCHEDULER_REPLAY_IMAGE`. `Dockerfile.replay` is parameterised; the defaults
target **mercury** (Laravel 12, PHP 8.4), which is the first application the replay sandbox is
being pointed at.

The shim itself is `../api/src/Replay/` — the reference implementation of the protocol in
`docs/specs/replay-chronos-replay-protocol.md`. This directory is only the packaging.

## Why the image is the application's image

The build context is the **application repository**, not the collector. The shim reaches the
image through the application's own Composer tree (`build-gaia/php-collector`, which mercury
already requires), so the replay image is the application image plus one ini file and one
entrypoint. Nothing about the application is rebuilt or re-resolved differently: a replay of code
that was built some other way would be replaying something else.

`auto_prepend_file` arms the shim, not a framework hook or a service provider. The recording has
to be discovered and the report armed *before* any application code runs — a fatal inside
Laravel's own bootstrap must still leave a report behind, or the operator sees a non-zero
container with no findings and reads it as a broken plan.

## What the sandbox imposes

From ADR 0015 and `scheduler/internal/executor/docker.go`, every replay container runs
`--read-only --network none --cap-drop ALL --security-opt no-new-privileges --user 65532:65532`,
with `/workspace` as the only writable bind mount and `/tmp` as a bounded `noexec` tmpfs. The
Dockerfile answers each of those:

| Constraint | What the image does |
| --- | --- |
| read-only rootfs | `bootstrap/cache` is populated at build (`package:discover`) and then made unwritable |
| writes must go to `/workspace` | `LARAVEL_STORAGE_PATH=/workspace/storage`, created by the entrypoint |
| `--user 65532:65532` | declared in the image, so a local `docker run` fails the same way the sandbox would |
| `--network none` | no build-time or run-time expectation of a reachable dependency; effects come from the recording |
| credential-shaped env is rejected | the image carries no `*_PASSWORD`/`*_SECRET`/`*_TOKEN` expectations; a replay does not connect to anything |
| a replay must not re-record | `CHRONOS_PHP_ENABLED=false`, `DD_TRACE_ENABLED=0` |

`APP_KEY` is generated at build time. It is not a secret: the container has no network, no
credentials and no real data, and a fixed key baked into a reviewable image would be worse.

## Building it

The base image must be pinned by digest, and `PHP_BASE_DIGEST` has no default so that the build
fails here rather than at launch — `executor.Docker` rejects a mutable tag with
`ErrMutableImage`. For the same reason the **result** has to be pushed: a `--load`ed local image
has no repository digest, so no digest-pinned reference to it can exist.

```sh
cd ~/chronos

# 1. Where mercury is, according to the compose setup that runs it locally.
MERCURY_DIR=$(grep -E '^MERCURY_DIR=' ~/code/docker-images/.env | cut -d= -f2-)
MERCURY_DIR=$(eval echo "$MERCURY_DIR")

# 2. Pin mercury's own base image by digest.
BASE=009103122343.dkr.ecr.eu-west-2.amazonaws.com/apps/mercury
aws ecr get-login-password --region eu-west-2 \
  | docker login --username AWS --password-stdin 009103122343.dkr.ecr.eu-west-2.amazonaws.com
BASE_DIGEST=$(docker buildx imagetools inspect "$BASE:base-8.4-trixie" --format '{{.Manifest.Digest}}')

# 3. Build and push, capturing the resulting digest.
REGISTRY=<a registry the execution plane can pull from>
docker buildx build \
  --platform linux/amd64 \
  --file collector/sdks/php/replay/Dockerfile.replay \
  --build-context chronos-replay=collector/sdks/php/replay \
  --build-arg PHP_BASE="$BASE" \
  --build-arg PHP_BASE_DIGEST="$BASE_DIGEST" \
  --tag "$REGISTRY/chronos-replay-mercury:$(git -C "$MERCURY_DIR" rev-parse --short HEAD)" \
  --metadata-file /tmp/chronos-replay-mercury.json \
  --push \
  "$MERCURY_DIR"

# 4. The digest-pinned reference the scheduler requires.
DIGEST=$(python3 -c 'import json;print(json.load(open("/tmp/chronos-replay-mercury.json"))["containerimage.digest"])')
echo "CHRONOS_SCHEDULER_REPLAY_IMAGE=$REGISTRY/chronos-replay-mercury@$DIGEST"
```

Set that variable on the scheduler (`scheduler/cmd/chronos-scheduler/main.go` reads it into
`replay.PlannerConfig.Image`) and restart it.

## Checking it without the scheduler

The shim needs nothing but a recording directory and the environment, so a materialised
recording can be replayed by hand:

```sh
docker run --rm --read-only --network none --user 65532:65532 \
  --mount "type=bind,src=$PWD/.local/replay/workspace,dst=/workspace" \
  --mount "type=bind,src=$PWD/.local/replay/recording,dst=/recorded/dst,readonly" \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=64m \
  --env CHRONOS_REPLAY_RECORDING --env CHRONOS_REPLAY_INPUTS \
  --env CHRONOS_REPLAY_EFFECT_DATABASE_WRITE --env CHRONOS_REPLAY_EFFECT_NETWORK_CALL \
  --env CHRONOS_REPLAY_EFFECT_QUEUE_PUBLISH \
  "$REGISTRY/chronos-replay-mercury@$DIGEST" \
  php /srv/www/mercury/local/artisan <the command the recording captured>
```

Exit code 0 is conformant, 90 diverged, 91 the plan or the mount was wrong, 92 the replay hit
something the recording could not answer. The report is
`/workspace/chronos-replay-report.json` either way.

**If the report is missing**, check `/workspace` first. The planner now creates lease workspaces
mode `0777` so uid `65532` can write the report when the scheduler uid differs. If an older
plane still has `0700` dirs owned by another user, the shim falls back to a single
`chronos-replay-report: ` line on stderr, which the executor's output bound may clip.



## Phase 1 acceptance (no mercury / no ECR)

ADR 0021 Phase 1 is proved by a fixture application that runs under the shim without a full
application image:

```sh
collector/sdks/php/replay/testdata/phase1/acceptance.sh
# or: php collector/sdks/php/api/tests/verify.php
```

The fixture calls `Effect::time`, prints `PHASE1_APP_RAN`, and requires a conformant
`chronos.replay.report.v1`. Digest-pinned mercury packaging above remains the production
`CHRONOS_SCHEDULER_REPLAY_IMAGE` path.

Workspace leases are created mode `0777` so uid `65532` can always write the report when the
planner uid differs from the sandbox uid.

## What the image does NOT do

It arms the protocol; it does not intercept anything on its own. Effects reach the shim only
through `Chronos\Collector\Replay\Effect` — a PDO wrapper, a Guzzle middleware or a native hook
has to call it. Until a PHP call path routes through `Effect`, a replay of mercury will run its
code and report `unconsumed_event` for everything the recording holds, which is a truthful
report of an unhooked runtime rather than a passing one. The interception surfaces still needed
are listed in `../api/src/Replay/README.md`.
