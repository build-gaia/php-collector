#!/usr/bin/env bash
# Build chronos.so for every PHP version the local app estate runs, and (with
# --distribute) drop each one into the app repo that needs it.
#
# WHY THREE BUILDS. A PHP extension is bound to the Zend module API of the PHP it
# was compiled against; the api directory name encodes it. The six local apps do
# not share one PHP, so one .so cannot serve them:
#
#   API 20210902  PHP 8.1  musl    service-pick-api, service-pack-api
#   API 20220829  PHP 8.2  glibc   deepwell
#   API 20240924  PHP 8.4  glibc   mercury
#   API 20240924  PHP 8.4  musl    service-auth, service-insights-api
#
# THE MODULE API ALONE DOES NOT IDENTIFY AN ARTEFACT. mercury and service-auth
# are both api 20240924, but mercury is Debian (glibc) and service-auth is Alpine
# (musl); a glibc .so simply does not load on musl, and the failure is SILENT —
# `extension_loaded()` returns false and PHP carries on. So every artefact is
# keyed by (php version, libc), built from Dockerfile.builder for glibc and
# Dockerfile.builder-alpine for musl. Verify a container's libc with:
#   docker exec <c> sh -c 'cat /etc/os-release | grep ^ID=; ldd --version 2>&1 | head -1'
#
# ARCHITECTURE — CHECK IT, DO NOT ASSUME. `--arch` selects it and defaults to
# amd64, because the ORIGINAL app containers (mercury, deepwell, service-*) are
# x86-64. The QLS estate under ~/code/qls/qls-development is NOT: those
# containers run aarch64 natively, and an amd64 .so copied into one does not
# load. The failure is SILENT — `extension_loaded()` returns false, PHP carries
# on, and telemetry simply stops (verified the hard way on 2026-09-08). Confirm
# with `docker exec <container> uname -m`, then build the matching arch:
#
#   ./docker-build-extensions.sh                          # x86-64 estate
#   ./docker-build-extensions.sh --arch arm64 --only musl  # QLS estate
#
# An arm64 artefact is named `chronos-php<v>-<libc>-arm64.so`; amd64 keeps the
# unsuffixed name so every existing reference to it still resolves.
#
# On Apple Silicon an amd64 build is emulated and SLOW — a release build with LTO
# takes tens of minutes per version. That is expected, not a hang.
#
# ext-php-rs 0.15 supports PHP 8.0..=8.4, so all three versions are in range.
set -euo pipefail

NATIVE="$(cd "$(dirname "$0")" && pwd)"
DIST="$NATIVE/dist"
DISTRIBUTE=0
ONLY=""
ARCH="amd64"
while [ $# -gt 0 ]; do
  case "$1" in
    --distribute) DISTRIBUTE=1 ;;
    --only) shift; ONLY="${1:-}" ;;
    --arch) shift; ARCH="${1:-}" ;;
    glibc|musl) ONLY="$1" ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
case "$ONLY" in ""|glibc|musl) ;; *) echo "--only takes glibc or musl, got '$ONLY'" >&2; exit 2 ;; esac
case "$ARCH" in
  amd64) PLATFORM="linux/amd64"; SUFFIX=""; ELF_PATTERN="ELF 64-bit.*x86-64" ;;
  arm64) PLATFORM="linux/arm64"; SUFFIX="-arm64"; ELF_PATTERN="ELF 64-bit.*aarch64" ;;
  *) echo "--arch takes amd64 or arm64, got '$ARCH'" >&2; exit 2 ;;
esac

# php version : libc : module api : repos needing it (space separated)
TARGETS=(
  "8.1:musl:20210902:service-pick-api service-pack-api"
  "8.2:glibc:20220829:deepwell"
  "8.3:musl:20230831:"
  "8.4:glibc:20240924:mercury"
  "8.4:musl:20240924:service-auth service-insights-api"
)

# The QLS estate lives somewhere else and is listed separately below. Rows above
# keep an empty repo list where the only consumers are QLS services, so the
# artefact is still BUILT — leaving a version out entirely is what made somebody
# build it by hand.
#
# QLS services are at $QLS_ROOT/<name>, not ~/code/<name>, and they run aarch64,
# so they need `--arch arm64`. Each entry is `name:php_version` because the
# estate is NOT on one PHP. These versions come from `php -v` in the RUNNING
# containers (confirmed 2026-09-09), not from the image tags: qls-oms and
# qls-admin are tagged `-php84` and run 8.3.31/8.3.33, so trusting the tag
# installs an 8.4 .so that never loads. libc is musl throughout (Alpine).
#
# CONFIRM AN ENTRY BEFORE TRUSTING IT. A wrong (version, libc, arch) triple
# installs a .so that silently never loads:
#
#   docker exec <container> php -v
#   docker exec <container> uname -m
#   docker exec <container> sh -c 'grep -m1 ^ID= /etc/os-release'
#
# qls-php (7.4) is deliberately absent: ext-php-rs 0.15 supports 8.0..=8.4, so
# there is no artefact that can serve it. That service is uninstrumentable until
# it moves to a supported PHP, and saying so here is better than a row that
# quietly copies a .so which cannot load.
QLS_ROOT="${QLS_ROOT:-$HOME/code/qls/qls-development/services}"
QLS_SERVICES=(
  "qls-oms:8.3"
  "qls-admin:8.3"
  "qls-auth:8.4"
  "qls-wms:8.4"
  "qls-statistics:8.4"
  "go-parcel:8.4"
  "qls-sorter:8.3"
  "qls-shipping:8.3"
  "qls-finance:8.3"
)

# --only glibc | --only musl restricts the run to one libc, so a partial rebuild
# does not pay for the emulated builds it already has.


mkdir -p "$DIST"

for target in "${TARGETS[@]}"; do
  PHP_V="${target%%:*}"
  rest="${target#*:}"
  LIBC="${rest%%:*}"
  rest="${rest#*:}"
  API="${rest%%:*}"
  if [ -n "$ONLY" ] && [ "$ONLY" != "$LIBC" ]; then
    echo "=== skipping PHP $PHP_V/$LIBC (--only $ONLY) ==="
    continue
  fi
  case "$LIBC" in
    glibc)
      DOCKERFILE="$NATIVE/Dockerfile.builder"
      EXTRA_RUSTFLAGS=""
      ;;
    musl)
      DOCKERFILE="$NATIVE/Dockerfile.builder-alpine"
      # rustup's host target on Alpine is x86_64-unknown-linux-musl, which
      # defaults to a STATIC crt and therefore refuses to emit a cdylib at all:
      #   "cannot produce cdylib ... target does not support these crate types".
      # A PHP extension is by definition a dynamic library, so static crt is
      # simply wrong here. This flag is what makes a musl .so possible.
      EXTRA_RUSTFLAGS="-C target-feature=-crt-static"
      ;;
    *) echo "unknown libc $LIBC"; exit 1 ;;
  esac
  echo "=== building chronos.so for PHP $PHP_V/$LIBC (module api $API) ==="
  docker build --platform "$PLATFORM" \
    -t "chronos-php-native-builder:$PHP_V-$LIBC" \
    --build-arg "PHP_VERSION=$PHP_V" \
    -f "$DOCKERFILE" "$NATIVE" >/dev/null

  # Build in a copy: the source mount is read-only and cargo needs to write target/.
  # The smoke test is the same recursion case used to validate the counted profile
  # on macOS — 200 calls to fact(12) must yield exactly 2400 invocations, and the
  # caller's exclusive time must equal its inclusive minus the callee's inclusive.
  # CACHES. The registry volume is shared by every target (crate sources are
  # arch- and version-independent); the target volume is per (version, libc,
  # arch) because the objects in it are not interchangeable. Without these each
  # build re-downloaded every crate and recompiled every dependency under LTO.
  docker run --rm --platform "$PLATFORM" \
    -v "$NATIVE:/src:ro" \
    -v "$DIST:/dist" \
    -v "chronos-ext-cargo-registry:/usr/local/cargo/registry" \
    -v "chronos-ext-target-$PHP_V-$LIBC-$ARCH:/tmp/native/target" \
    -e "EXTRA_RUSTFLAGS=$EXTRA_RUSTFLAGS" \
    "chronos-php-native-builder:$PHP_V-$LIBC" \
    bash -lc '
      set -euo pipefail
      # SOURCE ONLY. `cp -a /src /tmp/native` copied target/ (1.9 GB) and dist/
      # into the container on every build of every target, for cargo to ignore.
      mkdir -p /tmp/native
      cp -a /src/src /src/Cargo.toml /src/Cargo.lock /src/.cargo /tmp/native/
      cd /tmp/native
      export LIBCLANG_PATH=$(llvm-config --libdir 2>/dev/null || echo /usr/lib)
      [ -n "${EXTRA_RUSTFLAGS:-}" ] && export RUSTFLAGS="$EXTRA_RUSTFLAGS"
      cargo build --release
      EXT=$(find target/release -maxdepth 1 -name "libchronos.so" | head -1)
      test -n "$EXT" || { echo "no libchronos.so produced"; exit 1; }
      php -d "extension=$EXT" -r "exit(extension_loaded(\"chronos\") ? 0 : 1);"
      echo "  extension loads on $(php -r "echo PHP_VERSION;")"
      SPOOL=$(mktemp -d)
      php -d "extension=$EXT" -d chronos.enabled=1 -d chronos.organisation=smoke \
          -d chronos.project=p -d chronos.application=a \
          -d chronos.spool_directory=$SPOOL -d chronos.cli_enabled=1 \
          -r "function fact(\$n){ return \$n<=1 ? 1 : \$n*fact(\$n-1); } \$t=0; for(\$i=0;\$i<200;\$i++){ \$t+=fact(12); }" >/dev/null
      # ADR 0035: the collector no longer writes one file per document. Every
      # signal is appended as a frame to `segment-<generation>.spool`, so this
      # walks the log and pulls the `dprofile` frame out of it. Doing that here
      # means every artefact we ship has had its FRAME WRITER exercised, not
      # just its profiler — a torn or mis-encoded frame fails the build.
      D=$(find $SPOOL -name "segment-*.spool" | head -1)
      test -n "$D" || { echo "  FAIL: no spool segment emitted"; exit 1; }
      php -r "
        \$b = file_get_contents(\$argv[1]);
        \$o = 0; \$f = null; \$frames = 0; \$seen = [];
        while (\$o + 14 <= strlen(\$b)) {
          if (substr(\$b, \$o, 4) !== \"CHRN\") { fwrite(STDERR, \"  FAIL: bad frame magic at \$o\n\"); exit(1); }
          \$hl = unpack(\"v\", substr(\$b, \$o + 4, 2))[1];
          \$pl = unpack(\"V\", substr(\$b, \$o + 6, 4))[1];
          \$crc = unpack(\"V\", substr(\$b, \$o + 10, 4))[1];
          // The checksum covers header AND payload, in that order — the same
          // `body` the Rust and Go codecs hash. CRCing the payload alone reads
          // as a corrupt frame on every well-formed one.
          \$body = substr(\$b, \$o + 14, \$hl + \$pl);
          if (strlen(\$body) !== \$hl + \$pl) { fwrite(STDERR, \"  FAIL: truncated frame\n\"); exit(1); }
          \$head = json_decode(substr(\$body, 0, \$hl), true);
          \$pay = substr(\$body, \$hl);
          if (crc32(\$body) !== \$crc) { fwrite(STDERR, \"  FAIL: crc mismatch on a {\$head[\"signal\"]} frame\n\"); exit(1); }
          \$frames++; \$seen[] = \$head[\"signal\"] ?? \"?\";
          if ((\$head[\"signal\"] ?? \"\") === \"dprofile\") {
            \$d = json_decode(\$pay, true);
            foreach (\$d[\"functions\"] ?? [] as \$row) { if ((\$row[\"function\"] ?? \"\") === \"fact\") { \$f = \$row; } }
          }
          \$o += 14 + \$hl + \$pl;
        }
        if (\$o !== strlen(\$b)) { fwrite(STDERR, \"  FAIL: trailing bytes after the last frame\n\"); exit(1); }
        if (\$f === null) { fwrite(STDERR, \"  FAIL: no fact row in any dprofile frame (\$frames frame(s): \" . implode(\",\", \$seen) . \")\n\"); exit(1); }
        if ((int)\$f[\"callCount\"] !== 2400) { fwrite(STDERR, \"  FAIL: callCount {\$f[\"callCount\"]} != 2400\n\"); exit(1); }
        if ((int)\$f[\"maxRecursionDepth\"] !== 12) { fwrite(STDERR, \"  FAIL: depth {\$f[\"maxRecursionDepth\"]} != 12\n\"); exit(1); }
        if ((int)\$f[\"inclusiveNanoseconds\"] !== (int)\$f[\"exclusiveNanoseconds\"]) { fwrite(STDERR, \"  FAIL: recursion inflated inclusive time\n\"); exit(1); }
        echo \"  smoke ok: {\$frames} frame(s), fact calls={\$f[\"callCount\"]} depth={\$f[\"maxRecursionDepth\"]}\n\";
      " "$D"
      cp "$EXT" /dist/chronos-php'"$PHP_V"'-'"$LIBC$SUFFIX"'.so
    '
  file "$DIST/chronos-php$PHP_V-$LIBC$SUFFIX.so" | grep -q "$ELF_PATTERN" \
    || { echo "FAIL: $DIST/chronos-php$PHP_V-$LIBC$SUFFIX.so is not an $ARCH ELF"; exit 1; }
  echo "  -> $DIST/chronos-php$PHP_V-$LIBC$SUFFIX.so"
done

echo
if [ "$DISTRIBUTE" -eq 1 ]; then
  copied=0
  skipped=0
  # Copy one artefact into one service's extension directory, or account for why
  # it could not be. A missing destination is REPORTED AND COUNTED, never passed
  # over quietly: "distributed" printing while nothing moved is the same class of
  # silent no-op as a .so that never loads.
  place() {
    local php_v="$1" libc="$2" root="$3" name="$4" layout="${5:-code}"
    local artefact="$DIST/chronos-php$php_v-$libc$SUFFIX.so"
    local directory destination
    # Two estates, two conventions, and getting them the wrong way round places
    # nothing (or worse, places a file nothing reads):
    #   ~/code/*  : `.docker/extensions/chronos.so`, COPYed by Dockerfile.dev.
    #   QLS       : `docker/php/extensions/chronos-<arch>.so`, and the `dev`
    #               stage picks the one matching TARGETARCH. Both arches live
    #               side by side there, so the name MUST carry the arch.
    if [ "$layout" = "qls" ]; then
      directory="$root/$name/docker/php/extensions"
      destination="$directory/chronos-$ARCH.so"
    else
      directory="$root/$name/.docker/extensions"
      destination="$directory/chronos.so"
    fi
    if [ ! -f "$artefact" ]; then
      echo "SKIP $name: $artefact was not built (wrong --only/--arch for this run?)"
      skipped=$((skipped + 1))
      return
    fi
    if [ ! -d "$directory" ]; then
      echo "SKIP $name: no $directory"
      skipped=$((skipped + 1))
      return
    fi
    cp "$artefact" "$destination"
    echo "distributed PHP $php_v/$libc/$ARCH -> $name ($destination)"
    copied=$((copied + 1))
  }

  # The ~/code estate is amd64 and its layout keeps ONE unsuffixed chronos.so,
  # so there is no arch to discriminate on: an arm64 run would overwrite the
  # working amd64 artefact with one that cannot load, and nothing would say so
  # until telemetry quietly stopped. The QLS branch below is guarded the same
  # way in the opposite direction. Learned the hard way on 2026-09-09.
  if [ "$ARCH" != "amd64" ]; then
    echo
    echo "NOT distributing to ~/code services: those images are amd64 and this run"
    echo "built $ARCH. That layout has a single unsuffixed chronos.so, so writing"
    echo "an $ARCH one would REPLACE a working artefact with a silently dead one."
    echo "Re-run with: --arch amd64 --distribute"
  else
    for target in "${TARGETS[@]}"; do
      PHP_V="${target%%:*}"
      rest="${target#*:}"
      LIBC="${rest%%:*}"
      rest="${rest#*:}"
      REPOS="${rest#*:}"
      if [ -n "$ONLY" ] && [ "$ONLY" != "$LIBC" ]; then continue; fi
      for repo in $REPOS; do
        place "$PHP_V" "$LIBC" "$HOME/code" "$repo"
      done
    done
  fi

  # The QLS estate: musl, aarch64, mixed PHP versions, a different root.
  if [ -z "$ONLY" ] || [ "$ONLY" = "musl" ]; then
    if [ "$ARCH" != "arm64" ]; then
      echo
      echo "NOT distributing to QLS services: those containers are aarch64 and this"
      echo "run built $ARCH. An amd64 .so in an arm64 container never loads, and it"
      echo "fails silently. Re-run with: --arch arm64 --only musl --distribute"
    else
      echo
      for service in "${QLS_SERVICES[@]}"; do
        place "${service#*:}" "musl" "$QLS_ROOT" "${service%%:*}" "qls"
      done
    fi
  fi

  echo
  echo "distributed $copied artefact(s), skipped $skipped"
  echo "Rebuild the images to pick these up; Dockerfile.dev COPYs them at build time."
  echo "For a container whose image is built elsewhere, hot-copy instead:"
  echo "  docker cp <artefact> <container>:/usr/local/lib/php/extensions/no-debug-non-zts-<api>/chronos.so"
  echo "  docker restart <container>   # survives restart, lost on container recreation"
  # A distribution run that placed nothing did not do its job, whatever the
  # exit status would otherwise suggest.
  if [ "$copied" -eq 0 ]; then
    echo "FAIL: --distribute placed no artefacts at all" >&2
    exit 1
  fi
else
  echo "Built only. Re-run with --distribute to copy into the app repos."
fi
