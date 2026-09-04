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
# ARCHITECTURE. The app containers are x86-64 (verify: docker exec mercury uname -m),
# so every build is forced to linux/amd64. On Apple Silicon that is emulated and
# SLOW — a release build with LTO takes tens of minutes per version. That is
# expected, not a hang.
#
# ext-php-rs 0.15 supports PHP 8.0..=8.4, so all three versions are in range.
set -euo pipefail

NATIVE="$(cd "$(dirname "$0")" && pwd)"
DIST="$NATIVE/dist"
PLATFORM="linux/amd64"
DISTRIBUTE=0
ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --distribute) DISTRIBUTE=1 ;;
    --only) shift; ONLY="${1:-}" ;;
    glibc|musl) ONLY="$1" ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
case "$ONLY" in ""|glibc|musl) ;; *) echo "--only takes glibc or musl, got '$ONLY'" >&2; exit 2 ;; esac

# php version : libc : module api : repos needing it (space separated)
TARGETS=(
  "8.1:musl:20210902:service-pick-api service-pack-api"
  "8.2:glibc:20220829:deepwell"
  "8.4:glibc:20240924:mercury"
  "8.4:musl:20240924:service-auth service-insights-api"
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
  docker run --rm --platform "$PLATFORM" \
    -v "$NATIVE:/src:ro" \
    -v "$DIST:/dist" \
    -e "EXTRA_RUSTFLAGS=$EXTRA_RUSTFLAGS" \
    "chronos-php-native-builder:$PHP_V-$LIBC" \
    bash -lc '
      set -euo pipefail
      cp -a /src /tmp/native && cd /tmp/native
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
      D=$(find $SPOOL -name "*.dprofile" | head -1)
      test -n "$D" || { echo "  FAIL: no .dprofile emitted"; exit 1; }
      php -r "
        \$d = json_decode(file_get_contents(\$argv[1]), true);
        \$f = null;
        foreach (\$d[\"functions\"] ?? [] as \$row) { if ((\$row[\"function\"] ?? \"\") === \"fact\") { \$f = \$row; } }
        if (\$f === null) { fwrite(STDERR, \"  FAIL: no fact row\n\"); exit(1); }
        if ((int)\$f[\"callCount\"] !== 2400) { fwrite(STDERR, \"  FAIL: callCount {\$f[\"callCount\"]} != 2400\n\"); exit(1); }
        if ((int)\$f[\"maxRecursionDepth\"] !== 12) { fwrite(STDERR, \"  FAIL: depth {\$f[\"maxRecursionDepth\"]} != 12\n\"); exit(1); }
        if ((int)\$f[\"inclusiveNanoseconds\"] !== (int)\$f[\"exclusiveNanoseconds\"]) { fwrite(STDERR, \"  FAIL: recursion inflated inclusive time\n\"); exit(1); }
        echo \"  smoke ok: fact calls={\$f[\"callCount\"]} depth={\$f[\"maxRecursionDepth\"]}\n\";
      " "$D"
      cp "$EXT" /dist/chronos-php'"$PHP_V"'-'"$LIBC"'.so
    '
  file "$DIST/chronos-php$PHP_V-$LIBC.so" | grep -q "ELF 64-bit.*x86-64" \
    || { echo "FAIL: $DIST/chronos-php$PHP_V-$LIBC.so is not an x86-64 ELF"; exit 1; }
  echo "  -> $DIST/chronos-php$PHP_V-$LIBC.so"
done

echo
if [ "$DISTRIBUTE" -eq 1 ]; then
  for target in "${TARGETS[@]}"; do
    PHP_V="${target%%:*}"
    rest="${target#*:}"
    LIBC="${rest%%:*}"
    rest="${rest#*:}"
    REPOS="${rest#*:}"
    if [ -n "$ONLY" ] && [ "$ONLY" != "$LIBC" ]; then continue; fi
    for repo in $REPOS; do
      dest="$HOME/code/$repo/.docker/extensions/chronos.so"
      test -d "$(dirname "$dest")" || { echo "SKIP $repo: no .docker/extensions"; continue; }
      cp "$DIST/chronos-php$PHP_V-$LIBC.so" "$dest"
      echo "distributed PHP $PHP_V/$LIBC -> $repo"
    done
  done
  echo
  echo "Rebuild the images to pick these up; Dockerfile.dev COPYs them at build time."
else
  echo "Built only. Re-run with --distribute to copy into the app repos."
fi
