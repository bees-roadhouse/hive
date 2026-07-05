#!/usr/bin/env bash
set -euo pipefail

# Optional GUI stack for hosted sessions. Enabled by default so browser and
# desktop tests have a display without each agent reinventing Xvfb setup.
if [[ "${HIVE_GUI:-1}" == "1" ]]; then
  export DISPLAY="${DISPLAY:-:99}"
  export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/hive-gui/runtime}"
  mkdir -p "$XDG_RUNTIME_DIR"
  chmod 700 "$XDG_RUNTIME_DIR" || true

  if ! pgrep -f "Xvfb ${DISPLAY}" >/dev/null 2>&1; then
    Xvfb "$DISPLAY" -screen 0 "${HIVE_GUI_GEOMETRY:-1920x1080x24}" -ac +extension RANDR >/tmp/hive-gui/xvfb.log 2>&1 &
  fi

  if [[ "${HIVE_WINDOW_MANAGER:-1}" == "1" ]] && ! pgrep -x fluxbox >/dev/null 2>&1; then
    fluxbox >/tmp/hive-gui/fluxbox.log 2>&1 &
  fi

  if [[ "${HIVE_VNC:-0}" == "1" ]]; then
    if ! pgrep -f "x11vnc.*${DISPLAY}" >/dev/null 2>&1; then
      x11vnc -display "$DISPLAY" -forever -shared -nopw -rfbport "${HIVE_VNC_PORT:-5900}" >/tmp/hive-gui/x11vnc.log 2>&1 &
    fi
    if command -v websockify >/dev/null 2>&1 && [[ -d /usr/share/novnc ]] && ! pgrep -f "websockify.*${HIVE_NOVNC_PORT:-6080}" >/dev/null 2>&1; then
      websockify --web=/usr/share/novnc "${HIVE_NOVNC_PORT:-6080}" "localhost:${HIVE_VNC_PORT:-5900}" >/tmp/hive-gui/novnc.log 2>&1 &
    fi
  fi
fi

exec "$@"
