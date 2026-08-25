#!/bin/sh
# Prepare the writable workspace, then hand over to the command the execution plan named.
#
# The plan supplies the command (executor.Execution.Command plus Arguments), so this script must
# not decide what runs — it only makes the two directories Laravel needs before anything can
# boot, because the rest of the filesystem is read-only.
set -eu

storage="${LARAVEL_STORAGE_PATH:-/workspace/storage}"
mkdir -p \
	"${storage}/app" \
	"${storage}/framework/cache/data" \
	"${storage}/framework/sessions" \
	"${storage}/framework/testing" \
	"${storage}/framework/views" \
	"${storage}/logs"

# No command means nothing to replay. Exiting non-zero here is right: a plan that reaches this
# point is malformed, and a container that idles instead would hold its lease until the timeout.
if [ "$#" -eq 0 ]; then
	echo "chronos-replay: no command was supplied by the execution plan" >&2
	exit 64
fi

exec "$@"
