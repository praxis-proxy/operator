#!/usr/bin/env bash
#
# Throttles repetitive Gateway API conformance polling messages.
#
# The upstream helpers.go logs on every poll iteration (~500ms) while
# waiting for Gateways/Pods to become ready. This filter passes those
# lines at most once per THROTTLE_INTERVAL seconds (default 10).
# All other output passes through immediately.

set -euo pipefail

THROTTLE_INTERVAL="${THROTTLE_INTERVAL:-10}"

exec awk -v interval="$THROTTLE_INTERVAL" '
{
    if ($0 ~ /was not in conditions list/ ||
        $0 ~ /condition set to .*, expected/ ||
        $0 ~ /not Accepted yet/ ||
        $0 ~ /not Programmed yet/ ||
        $0 ~ /not ready yet/) {
        now = systime()
        if (now - last_print >= interval) {
            print
            last_print = now
            fflush()
        } else {
            suppressed++
        }
    } else {
        print
        fflush()
    }
}
END {
    if (suppressed > 0) {
        printf "[conformance-filter] Throttled %d repetitive polling messages (1/%ds)\n", suppressed, interval
    }
}'
