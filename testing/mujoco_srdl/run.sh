#!/usr/bin/env bash
# SPDX-License-Identifier: MulanPSL-2.0
set -euo pipefail

here="$(cd -- "$(dirname -- "$0")" && pwd)"
repo="$(cd -- "$here/../.." && pwd)"
deployment="${MUJOCO_DEPLOYMENT:-$(dirname -- "$repo")/mujoco-ranger-piper}"
if (( $# == 0 )); then
    set -- 4.783 1.436 1.0472
elif (( $# != 3 )); then
    echo "Usage: $0 [GOAL_X GOAL_Y GOAL_YAW]" >&2
    exit 2
fi
cd "$deployment"
source ./lab-env.sh
compose=(docker compose -f sim/compose.yaml -f sim/compose.vm-native.yaml)
if ss -ltn | grep -E ':(50051|50061|50062|50091)[[:space:]]'; then
    echo "Stop the existing Robonix deployment before starting the SRDL test." >&2
    exit 1
fi

mkdir -p "$here/report"
run="$(mktemp -d "$here/report/run.XXXXXX")"
manifest="$(mktemp --suffix=.yaml "$PWD/.srdl-test.XXXXXX")"
boot_pid=
started_bridge=false
cleanup() {
    if [[ -n "$boot_pid" ]]; then
        kill -TERM "$boot_pid" 2>/dev/null || true
        wait "$boot_pid" 2>/dev/null || true
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

python3 - "$manifest" "$here/navigation.srdl" <<'PY'
import re
import sys
from pathlib import Path

source = Path("robonix_srdl_manifest.yaml").read_text()
updated, count = re.subn(r"(?m)^(\s+sentinel_rules: ).+$", lambda match: match[1] + sys.argv[2], source)
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
    --deployment "$deployment" --goal "$@" --output "$run/cases"
