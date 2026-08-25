#!/usr/bin/env bash
# Build chronos.so against PHP 8.4 in Docker and run Phase 1b scalar interception acceptance.
set -euo pipefail
NATIVE="$(cd "$(dirname "$0")" && pwd)"
PHP_SDK="$(cd "$NATIVE/.." && pwd)"
IMAGE="chronos-php-native-builder:8.4"

docker build -t "$IMAGE" --build-arg PHP_VERSION=8.4 -f "$NATIVE/Dockerfile.builder" "$NATIVE"

docker run --rm \
  -v "$NATIVE:/build/native:ro" \
  -v "$PHP_SDK:/build/php:ro" \
  -w /build/native \
  "$IMAGE" \
  bash -lc '
    set -euo pipefail
    cp -a /build/native /tmp/native && cd /tmp/native
    export LIBCLANG_PATH=$(llvm-config --libdir 2>/dev/null || echo /usr/lib)
    cargo build --release
    EXT=$(find target/release -maxdepth 1 \( -name "libchronos.so" -o -name "libchronos.dylib" \) | head -1)
    test -n "$EXT"
    cp "$EXT" /tmp/chronos.so
    php -d "extension=/tmp/chronos.so" -r "exit(extension_loaded(\"chronos\")?0:1);"
    WORK=$(mktemp -d)
    REPORT="$WORK/chronos-replay-report.json"
    OUT=$(
      CHRONOS_REPLAY_RECORDING=rec-phase1b \
      CHRONOS_REPLAY_INPUTS=/build/php/replay/testdata/phase1b/recording \
      CHRONOS_REPLAY_REPORT="$REPORT" \
      CHRONOS_REPLAY_EFFECT_DATABASE_WRITE=blocked \
      CHRONOS_REPLAY_EFFECT_NETWORK_CALL=blocked \
      CHRONOS_REPLAY_EFFECT_QUEUE_PUBLISH=blocked \
      CHRONOS_PHP_ENABLED=false \
      php -d "extension=/tmp/chronos.so" \
          -d "auto_prepend_file=/build/php/api/src/Replay/bootstrap.php" \
          /build/php/replay/testdata/phase1b/app.php
    )
    echo "$OUT"
    echo "$OUT" | grep -q "PHASE1B_APP_RAN time=1755772800 rand=42"
    test -f "$REPORT"
    php -r "\$r=json_decode(file_get_contents(\$argv[1]), true); exit((\$r[\"outcome\"] ?? \"\") === \"conformant\" ? 0 : 1);" "$REPORT"
    echo "PASS phase1b scalar builtin interception"
  '
