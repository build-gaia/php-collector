#!/usr/bin/env bash
# ADR 0021 Phase 1: application code runs under the replay shim and leaves a protocol report.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
SHIM="$(cd "$ROOT/../../../api/src/Replay" && pwd)/bootstrap.php"
WORKSPACE="$(mktemp -d "${TMPDIR:-/tmp}/chronos-phase1.XXXXXX")"
trap 'rm -rf "$WORKSPACE"' EXIT

mkdir -p "$WORKSPACE"
REPORT="$WORKSPACE/chronos-replay-report.json"

OUTPUT="$(
  CHRONOS_REPLAY_RECORDING=rec-phase1 \
  CHRONOS_REPLAY_INPUTS="$ROOT/recording" \
  CHRONOS_REPLAY_REPORT="$REPORT" \
  CHRONOS_REPLAY_EFFECT_DATABASE_WRITE=blocked \
  CHRONOS_REPLAY_EFFECT_NETWORK_CALL=blocked \
  CHRONOS_REPLAY_EFFECT_QUEUE_PUBLISH=blocked \
  CHRONOS_PHP_ENABLED=false \
  php -d "auto_prepend_file=$SHIM" -d display_errors=Off -d log_errors=On \
    "$ROOT/app.php"
)"

echo "$OUTPUT" | grep -q 'PHASE1_APP_RAN result=1755772800' \
  || { echo "missing application marker: $OUTPUT" >&2; exit 1; }

test -f "$REPORT" || { echo "missing report at $REPORT" >&2; exit 1; }

php -r '
$report = json_decode(file_get_contents($argv[1]), true, 512, JSON_THROW_ON_ERROR);
if (($report["schema"] ?? "") !== "chronos.replay.report.v1") {
    fwrite(STDERR, "unexpected schema\n");
    exit(1);
}
if (($report["outcome"] ?? "") !== "conformant") {
    fwrite(STDERR, "outcome=" . ($report["outcome"] ?? "") . " want conformant\n");
    exit(1);
}
' "$REPORT"

echo "PASS phase1 application executed under replay shim"
