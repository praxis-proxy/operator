#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Retry
# ---------------------------------------------------------------------------
#
# Runs a command, retrying with exponential backoff until it succeeds or the
# attempts run out.
#
# Every network fetch in CI is a coin flip that occasionally lands wrong: a
# registry hiccup or a dropped connection to a download host fails a job that
# has nothing to do with the change under test, and the only fix available is
# a human clicking re-run. Wrapping those fetches here turns a transient
# failure into a pause.
#
# Deliberately not used for anything but fetches. Retrying a test suite would
# hide flakiness that is worth seeing, and retrying a mutation would apply it
# twice.
#
# Usage:
#   hack/retry.sh curl -fsSL -o ./kind https://example.com/kind
#   RETRY_ATTEMPTS=3 hack/retry.sh docker pull image:tag

ATTEMPTS="${RETRY_ATTEMPTS:-5}"
DELAY="${RETRY_DELAY:-2}"

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <command> [args...]" >&2
    exit 2
fi

attempt=1
while true; do
    # Captured on the failure branch rather than read after an `if`, where
    # `$?` is the status of the `if` itself and reads 0 however the command
    # exited — which would report a permanently failing fetch as a success.
    status=0
    "$@" || status="$?"

    if [ "${status}" -eq 0 ]; then
        exit 0
    fi

    if [ "${attempt}" -ge "${ATTEMPTS}" ]; then
        echo "==> giving up on '$*' after ${attempt} attempts (exit ${status})" >&2
        exit "${status}"
    fi

    echo "==> '$*' failed with exit ${status}; retrying in ${DELAY}s (attempt ${attempt}/${ATTEMPTS})" >&2
    sleep "${DELAY}"
    attempt=$((attempt + 1))
    DELAY=$((DELAY * 2))
done
