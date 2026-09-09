#!/usr/bin/env bash
# Build chronos.so for the QLS local estate (~/code/qls/qls-development/services)
# and, with --distribute, commit each artefact into the service repo that needs it.
#
# WHY THIS IS A SEPARATE SCRIPT FROM docker-build-extensions.sh. That one serves the
# JJ estate: amd64 only, glibc and musl mixed, distributed as a single
# `.docker/extensions/chronos.so` per repo. The QLS estate differs on all three axes:
#
#   - EVERY service is Alpine (musl) on aarch64. Verify:
#       docker exec qls-auth-php-1 sh -c 'cat /etc/os-release | grep ^ID=; uname -m'
#   - TWO module APIs are in play, because the services are not on one PHP:
#       API 20230831  PHP 8.3  musl   qls-admin qls-finance qls-oms qls-shipping qls-sorter
#       API 20240924  PHP 8.4  musl   qls-auth go-parcel qls-statistics qls-wms
#     A .so is bound to the Zend module API it was compiled against, and the mismatch
#     failure is SILENT — `extension_loaded()` returns false and PHP carries on with
#     no telemetry. So the split is load-bearing, not tidiness.
#   - Each service Dockerfile selects its artefact by BUILD ARCH:
#       COPY docker/php/extensions/ ... chronos-${TARGETARCH}.so
#     so a repo carries chronos-arm64.so AND chronos-amd64.so, and both must be
#     rebuilt together or a colleague on linux/amd64 silently gets the older build.
#
# qls-web is deliberately absent: it runs PHP 7.4, and ext-php-rs supports 8.0..=8.4.
#
# ARCHITECTURE COST. arm64 builds are native on Apple Silicon and quick. amd64 is
# emulated, and this crate's release profile is lto=true + codegen-units=1, so an
# emulated build takes tens of minutes per PHP version. That is expected, not a hang.
# Default is both; --arch arm64 restricts to what actually runs locally, and
# --no-build distributes artefacts already in dist/ without paying to rebuild them.
set -euo pipefail

NATIVE="$(cd "$(dirname "$0")" && pwd)"
DIST="$NATIVE/dist"
ESTATE="${QLS_SERVICES_DIR:-$HOME/code/qls/qls-development/services}"
DISTRIBUTE=0
BUILD=1
ARCHES="arm64 amd64"
ONLY_PHP=""
while [ $# -gt 0 ]; do
  case "$1" in
    --distribute) DISTRIBUTE=1 ;;
    # Distribute what is already in dist/ without rebuilding it. Without this,
    # `--distribute` pays for a full rebuild: cargo's target/ lives inside the
    # throwaway container copy, so nothing is cached between runs and every
    # invocation is a cold lto=true build.
    --no-build) BUILD=0 ;;
    --arch) shift; ARCHES="${1:-}" ;;
    --php) shift; ONLY_PHP="${1:-}" ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
for a in $ARCHES; do
  case "$a" in arm64|amd64) ;; *) echo "--arch takes arm64 and/or amd64, got '$a'" >&2; exit 2 ;; esac
done

# php version : module api : service repos needing it (space separated)
TARGETS=(
  "8.3:20230831:qls-admin qls-finance qls-oms qls-shipping qls-sorter"
  "8.4:20240924:qls-auth go-parcel qls-statistics qls-wms"
)

mkdir -p "$DIST"

for target in "${TARGETS[@]}"; do
  [ "$BUILD" -eq 1 ] || break
  PHP_V="${target%%:*}"
  rest="${target#*:}"
  API="${rest%%:*}"
  if [ -n "$ONLY_PHP" ] && [ "$ONLY_PHP" != "$PHP_V" ]; then
    echo "=== skipping PHP $PHP_V (--php $ONLY_PHP) ==="
    continue
  fi
  for ARCH in $ARCHES; do
    PLATFORM="linux/$ARCH"
    echo "=== building chronos.so for PHP $PHP_V/musl/$ARCH (module api $API) ==="
    docker build --platform "$PLATFORM" \
      -t "chronos-php-native-builder:$PHP_V-musl-$ARCH" \
      --build-arg "PHP_VERSION=$PHP_V" \
      -f "$NATIVE/Dockerfile.builder-alpine" "$NATIVE" >/dev/null

    # Build in a copy: the source mount is read-only and cargo needs to write target/.
    # The smoke test is the same recursion case the JJ script uses — 200 calls to
    # fact(12) must yield exactly 2400 invocations, and a recursive frame must not
    # inflate its own inclusive time. It runs INSIDE the target container, so it also
    # proves the artefact actually loads on that PHP rather than only that it linked.
    docker run --rm --platform "$PLATFORM" \
      -v "$NATIVE:/src:ro" \
      -v "$DIST:/dist" \
      "chronos-php-native-builder:$PHP_V-musl-$ARCH" \
      bash -lc '
        set -euo pipefail
        cp -a /src /tmp/native && cd /tmp/native
        export LIBCLANG_PATH=$(llvm-config --libdir 2>/dev/null || echo /usr/lib)
        # rustup on Alpine targets *-unknown-linux-musl, which defaults to a STATIC
        # crt and therefore refuses to emit a cdylib at all. A PHP extension is by
        # definition a dynamic library, so this flag is what makes a musl .so possible.
        export RUSTFLAGS="-C target-feature=-crt-static"
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
        cp "$EXT" /dist/chronos-php'"$PHP_V"'-musl-'"$ARCH"'.so
      '
    OUT="$DIST/chronos-php$PHP_V-musl-$ARCH.so"
    case "$ARCH" in
      arm64) EXPECT="ARM aarch64" ;;
      amd64) EXPECT="x86-64" ;;
    esac
    file "$OUT" | grep -q "ELF 64-bit.*$EXPECT" \
      || { echo "FAIL: $OUT is not an $EXPECT ELF: $(file "$OUT")"; exit 1; }
    echo "  -> $OUT"
  done
done

echo
if [ "$DISTRIBUTE" -eq 1 ]; then
  for target in "${TARGETS[@]}"; do
    PHP_V="${target%%:*}"
    rest="${target#*:}"
    REPOS="${rest#*:}"
    if [ -n "$ONLY_PHP" ] && [ "$ONLY_PHP" != "$PHP_V" ]; then continue; fi
    for repo in $REPOS; do
      dir="$ESTATE/$repo/docker/php/extensions"
      test -d "$dir" || { echo "SKIP $repo: no $dir"; continue; }
      for ARCH in $ARCHES; do
        src="$DIST/chronos-php$PHP_V-musl-$ARCH.so"
        test -f "$src" || { echo "FAIL: $src missing (build it, or drop --no-build)" >&2; exit 1; }
        cp "$src" "$dir/chronos-$ARCH.so"
      done
      echo "distributed PHP $PHP_V/musl [$ARCHES] -> $repo"
    done
  done
else
  echo "artefacts in $DIST (pass --distribute to copy them into the service repos)"
fi
