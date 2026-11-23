#!/usr/bin/env bash
# SPDX-License-Identifier: MulanPSL-2.0
set -euo pipefail

here="$(cd -- "$(dirname -- "$0")" && pwd)"
repo="$(cd -- "$here/../.." && pwd)"
deployment="${MUJOCO_DEPLOYMENT:-$(dirname -- "$repo")/mujoco-ranger-piper}"
summarize=
case "${1:-}" in
    --summarize) summarize=stub; shift ;;
    --summarize-live) summarize=live; shift ;;
esac
if [[ "$summarize" == live ]]; then
    : "${SENTINEL_SUMMARIZE_BASE_URL:?Set SENTINEL_SUMMARIZE_BASE_URL}"
    : "${SENTINEL_SUMMARIZE_MODEL:?Set SENTINEL_SUMMARIZE_MODEL}"
    : "${SENTINEL_SUMMARIZE_API_KEY:?Set SENTINEL_SUMMARIZE_API_KEY}"
fi
if (( $# == 0 )); then
    set -- 4.783 1.436 1.0472
elif (( $# != 3 )); then
    echo "Usage: $0 [--summarize|--summarize-live] [GOAL_X GOAL_Y GOAL_YAW]" >&2
    exit 2
fi
cd "$deployment"
source ./lab-env.sh
compose=(docker compose -f sim/compose.yaml -f sim/compose.vm-native.yaml)
if ss -ltn | grep -E ':(50051|50061|50062|50091)[[:space:]]'; then
    echo "Stop the existing Robonix deployment before starting the SRDL test." >&2
    exit 1
fi

report_root="${SRDL_REPORT_ROOT:-$here/report}"
mkdir -p "$report_root"
run="$(mktemp -d "$report_root/run.XXXXXX")"
manifest="$(mktemp --suffix=.yaml "$PWD/.srdl-test.XXXXXX")"
boot_pid=
model_pid=
started_bridge=false
cleanup() {
    if [[ -n "$boot_pid" ]]; then
        kill -TERM "$boot_pid" 2>/dev/null || true
        wait "$boot_pid" 2>/dev/null || true
    fi
    if [[ -n "$model_pid" ]]; then
        kill -TERM "$model_pid" 2>/dev/null || true
        wait "$model_pid" 2>/dev/null || true
    fi
    rm -f -- "$manifest"
    if [[ "$started_bridge" == true ]]; then
        "${compose[@]}" stop bridge >>"$run/bridge.log" 2>&1
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Artifacts at $run"
if [[ "$(docker inspect "$ROBONIX_SIM_CONTAINER" -f '{{.State.Running}}' 2>/dev/null || true)" != true ]]; then
    "${compose[@]}" up -d --no-build >"$run/bridge.log" 2>&1
    started_bridge=true
fi
ready=false
for _ in $(seq 1 100); do
    if curl -fsS --max-time 2 http://127.0.0.1:8766/health 2>/dev/null \
        | python3 -c 'import json, sys; h = json.load(sys.stdin); sys.exit(not (h.get("backend") == "native" and h.get("runtimeConnected") and h.get("environment") == sys.argv[1] and h["frames"]["state"] > 0))' "$SIM_ENVIRONMENT" 2>/dev/null; then
        ready=true
        break
    fi
    sleep 0.5
done
if [[ "$ready" != true ]]; then
    docker logs --tail 60 "$ROBONIX_SIM_CONTAINER" >>"$run/bridge.log" 2>&1 || true
    echo "Native MuJoCo unavailable. See $run/bridge.log" >&2
    exit 1
fi

rules="$here/navigation.srdl"
test_args=()
if [[ -n "$summarize" ]]; then
    rules="$here/summarize.srdl"
    test_args+=(--summarize)
    if [[ "$summarize" == stub ]]; then
        python3 "$here/summarize_model.py" "$run/model-url" >"$run/model.jsonl" 2>"$run/model.log" &
        model_pid=$!
        for _ in $(seq 1 50); do
            [[ -s "$run/model-url" ]] && break
            kill -0 "$model_pid" 2>/dev/null || break
            sleep 0.1
        done
        if [[ ! -s "$run/model-url" ]]; then
            echo "Summarize test model did not start. See $run/model.log" >&2
            exit 1
        fi
        export SENTINEL_SUMMARIZE_BASE_URL="$(cat "$run/model-url")"
        export SENTINEL_SUMMARIZE_MODEL=mujoco-srdl-test
        export SENTINEL_SUMMARIZE_API_KEY=mujoco-srdl-test
        test_args+=(--model-trace "$run/model.jsonl")
    fi
fi

python3 - "$manifest" "$rules" "$summarize" <<'PY'
import re
import sys
from pathlib import Path

source = Path("robonix_srdl_manifest.yaml").read_text()
replacement = sys.argv[2]
if sys.argv[3]:
    replacement += """
    sentinel_strategies:
      Summarize:
        api_base: "${SENTINEL_SUMMARIZE_BASE_URL}"
        model: "${SENTINEL_SUMMARIZE_MODEL}"
        api_key: "${SENTINEL_SUMMARIZE_API_KEY}" """
updated, count = re.subn(r"(?m)^(\s+sentinel_rules: ).+$", lambda match: match[1] + replacement, source)
if count != 1:
    raise RuntimeError("The MuJoCo manifest must declare one sentinel_rules file")
Path(sys.argv[1]).write_text(updated)
PY
timeout --kill-after=30s 240s rbnx boot -f "$manifest" --no-update-check --log-dir "$run/components" --verbose \
    >"$run/boot.log" 2>&1 &
boot_pid=$!
ready=false
for _ in $(seq 1 120); do
    if grep -qF "Ctrl-C to tear down" "$run/boot.log"; then
        ready=true
        break
    fi
    kill -0 "$boot_pid" 2>/dev/null || break
    sleep 1
done
if [[ "$ready" != true ]]; then
    tail -20 "$run/boot.log" >&2
    exit 1
fi

"$deployment/.venv-codegen/bin/python" "$here/run.py" \
    --deployment "$deployment" --goal "$@" --output "$run/cases" "${test_args[@]}"
