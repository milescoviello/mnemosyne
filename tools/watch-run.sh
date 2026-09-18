#!/usr/bin/env bash
# Watch a GitHub Actions run and report each job as it changes.
#
#   tools/watch-run.sh <run-id> [poll-seconds] [give-up-minutes]
#
# Prints a line only when something actually changes, so the log stays
# readable over a long wait, and exits non-zero if the run fails or the
# deadline passes.
set -uo pipefail
run="${1:?usage: watch-run.sh <run-id> [poll] [deadline-min]}"
poll="${2:-30}"
deadline_min="${3:-90}"
started=$(date +%s)
declare -A seen

stamp() { date -u +%H:%M:%S; }

while :; do
    json=$(gh run view "$run" --json status,conclusion,jobs 2>/dev/null) || {
        echo "$(stamp)  cannot reach GitHub, retrying"; sleep "$poll"; continue; }

    while IFS=$'\t' read -r name state; do
        if [ "${seen[$name]:-}" != "$state" ]; then
            echo "$(stamp)  $state  $name"
            seen[$name]="$state"
        fi
    done < <(echo "$json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
for j in d["jobs"]:
    # removeprefix, not lstrip: lstrip takes a character set, so it ate the
    # u, b and t out of "ubuntu-latest"
    short = j["name"].split(",")[0].removeprefix("build (").strip()
    state = j.get("conclusion") or j["status"]
    print(short + chr(9) + state)')

    status=$(echo "$json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')
    if [ "$status" = "completed" ]; then
        concl=$(echo "$json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["conclusion"] or "?")')
        echo "$(stamp)  run finished: $concl"
        [ "$concl" = "success" ] || exit 1
        exit 0
    fi

    now=$(date +%s)
    if [ $(( (now - started) / 60 )) -ge "$deadline_min" ]; then
        echo "$(stamp)  gave up after ${deadline_min} minutes; still: $status"
        exit 2
    fi
    sleep "$poll"
done
