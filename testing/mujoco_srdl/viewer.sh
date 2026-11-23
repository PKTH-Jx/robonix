#!/usr/bin/env bash
# SPDX-License-Identifier: MulanPSL-2.0
set -euo pipefail

reset=false
case "$#:${1:-}" in
    0:) ;;
    1:--reset) reset=true ;;
    *) echo "Usage: $0 [--reset]" >&2; exit 2 ;;
esac

repo="$(cd -- "$(dirname -- "$0")/../.." && pwd)"
deployment="${MUJOCO_DEPLOYMENT:-$(dirname -- "$repo")/mujoco-ranger-piper}"
cd "$deployment"
source ./lab-env.sh

export DISPLAY=:0
export MUJOCO_DEMO_XAUTHORITY
MUJOCO_DEMO_XAUTHORITY="$(ps -u "$(id -u)" -o args= | awk -v display="$DISPLAY" '
    $1 ~ /\/Xwayland$/ && $2 == display {
        for (i = 3; i < NF; i++) if ($i == "-auth") { print $(i + 1); exit }
    }
')"
if [[ ! -r "$MUJOCO_DEMO_XAUTHORITY" ]]; then
    echo "Log in to the robonix-vm desktop before starting the Viewer." >&2
    exit 1
fi
export XAUTHORITY="$MUJOCO_DEMO_XAUTHORITY"

running="$(docker inspect -f '{{.State.Running}}' "$ROBONIX_SIM_CONTAINER" 2>/dev/null || true)"
if [[ "$running" == true && "$reset" == false ]]; then
    inspect='{{range .Config.Env}}{{println .}}{{end}}'
    if ! docker inspect -f "$inspect" "$ROBONIX_SIM_CONTAINER" | grep -x 'SIM_HEADLESS=0' >/dev/null; then
        echo "The simulation is running headless. Use --reset to restart it with the Viewer." >&2
        exit 1
    fi
else
    if ss -ltn | grep -E ':(50051|50061|50062|50091)[[:space:]]'; then
        echo "Stop the Robonix deployment before starting or resetting MuJoCo." >&2
        exit 1
    fi
    docker compose -f sim/compose.yaml -f sim/compose.vm-native.yaml -f - \
        up -d --no-build --force-recreate <<'YAML'
services:
  bridge:
    environment:
      SIM_HEADLESS: "0"
      SIM_GPU_REQUIRED: "0"
      DISPLAY: ":0"
      WAYLAND_DISPLAY: ""
      MUJOCO_GL: glfw
      XAUTHORITY: /tmp/vm-display-authority
    volumes:
      - /tmp/.X11-unix:/tmp/.X11-unix:ro
      - type: bind
        source: ${MUJOCO_DEMO_XAUTHORITY}
        target: /tmp/vm-display-authority
        read_only: true
YAML
fi

for _ in $(seq 1 100); do
    if curl -fsS --max-time 2 http://127.0.0.1:8766/health 2>/dev/null \
        | python3 -c 'import json, sys; h = json.load(sys.stdin); sys.exit(not (h.get("backend") == "native" and h.get("runtimeConnected") and h.get("environment") == sys.argv[1] and h["frames"]["state"] > 0))' "$SIM_ENVIRONMENT" 2>/dev/null; then
        if window="$(xwininfo -root -tree 2>/dev/null | grep -F '"MuJoCo : ')"; then
            printf '%s\n' "$window"
            echo "MuJoCo Viewer is ready on the VM desktop."
            exit 0
        fi
    fi
    sleep 0.5
done
docker logs --tail 40 "$ROBONIX_SIM_CONTAINER"
echo "MuJoCo Viewer did not become ready." >&2
exit 1
