#!/usr/bin/env bash
# SPDX-License-Identifier: MulanPSL-2.0
set -euo pipefail

here="$(cd -- "$(dirname -- "$0")" && pwd)"
repo="$(cd -- "$here/../.." && pwd)"
deployment="${MUJOCO_DEPLOYMENT:-$(dirname -- "$repo")/mujoco-ranger-piper}"
usage() {
    cat <<'HELP'
Usage: bash testing/mujoco_srdl/suite.sh [direct|stub|live|all] [--viewer] [--local]

  direct   Check before, while and after with deterministic SRDL rules.
  stub     Exercise Summarize with controlled model responses.
  live     Exercise Summarize with the configured real model.
  all      Run all three groups (default).

  --viewer Reset the MuJoCo scene and show the Viewer before each group.
  --local  With all, run direct and stub only.
HELP
}

group=
local_only=false
viewer=false
for argument in "$@"; do
    case "$argument" in
        direct|stub|live|all)
            if [[ -n "$group" ]]; then usage >&2; exit 2; fi
            group="$argument"
            ;;
        --local) local_only=true ;;
        --viewer) viewer=true ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

group="${group:-all}"
if [[ "$local_only" == true && "$group" != all ]]; then
    echo "Use --local with all." >&2
    exit 2
fi

if [[ "$viewer" == false ]]; then
    container="$(cd "$deployment" && source ./lab-env.sh && printf '%s' "$ROBONIX_SIM_CONTAINER")"
    if [[ "$(docker inspect -f '{{.State.Running}}' "$container" 2>/dev/null || true)" == true ]]; then
        echo "Close the current simulation or use --viewer to reset it before each group." >&2
        exit 1
    fi
fi

mkdir -p "$here/report"
output="$(mktemp -d "$here/report/suite.XXXXXX")"
summary="$output/summary.md"
failed=false
printf '# MuJoCo SRDL comparison\n\n| Group | Result | Evidence |\n| --- | --- | --- |\n' >"$summary"
echo "Suite artifacts at $output"

execute_group() {
    local directory="$1"
    shift
    if [[ "$viewer" == true ]]; then
        bash "$here/viewer.sh" --reset || return
    fi
    SRDL_REPORT_ROOT="$directory" bash "$here/run.sh" "$@"
}

run_group() {
    local name="$1" result evidence
    shift
    mkdir -p "$output/$name"
    if execute_group "$output/$name" "$@" 2>&1 | tee "$output/$name/console.log"; then
        result=PASS
    else
        result=FAIL
        failed=true
    fi
    evidence="$(find "$output/$name" -path '*/cases/summary.md' -type f -print -quit)"
    if [[ -n "$evidence" ]]; then
        evidence="${evidence#"$output/"}"
    else
        evidence="$name/console.log"
    fi
    printf '| %s | %s | [Details](%s) |\n' "$name" "$result" "$evidence" >>"$summary"
}

if [[ "$group" == all || "$group" == direct ]]; then
    run_group direct
fi
if [[ "$group" == all || "$group" == stub ]]; then
    run_group stub --summarize
fi
if [[ "$group" == all || "$group" == live ]]; then
    if [[ "$local_only" == true ]]; then
        printf '| live | SKIP | Local-only run |\n' >>"$summary"
    elif [[ -z "${SENTINEL_SUMMARIZE_BASE_URL:-}" || -z "${SENTINEL_SUMMARIZE_MODEL:-}" || -z "${SENTINEL_SUMMARIZE_API_KEY:-}" ]]; then
        printf '| live | SKIP | Missing model configuration in the current shell |\n' >>"$summary"
        failed=true
    else
        run_group live --summarize-live
    fi
fi

printf '\n'
cat "$summary"
[[ "$failed" == false ]]
