#!/bin/sh
# Run the GUI smoke test on its own display and session bus, so it cannot reach a
# running copy of the app, the desktop's panel or the user's windows.
#
#     crates/ylr-auto/tests/run_gui_smoke.sh [folder for screenshots]
set -eu

display_number=57
screenshots=${1:-}

cargo test -q -p ylr-auto --test gui_smoke --no-run

gtk4-broadwayd --address 127.0.0.1 ":$display_number" >/dev/null 2>&1 &
broadway_pid=$!
trap 'kill "$broadway_pid" 2>/dev/null || true' EXIT
sleep 1

GDK_BACKEND=broadway BROADWAY_DISPLAY=":$display_number" GSK_RENDERER=cairo \
YLR_AUTO_GUI_SMOKE=1 YLR_AUTO_SCREENSHOTS="$screenshots" \
    dbus-run-session -- cargo test -q -p ylr-auto --test gui_smoke
