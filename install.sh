#!/bin/sh
# Install agent-relay as ~/.local/bin/agent-relay (or the path given).
#
# The installed copy's first line names the Python.app binary behind
# /usr/bin/python3, not /usr/bin/python3 itself: that stub adds SDKROOT, CPATH,
# LIBRARY_PATH and MANPATH to the environment, which agent-relay would pass on
# to Claude and every build Claude runs.
set -eu
python=$(/usr/bin/python3 -I -c 'import sys; print(sys.prefix + "/Resources/Python.app/Contents/MacOS/Python")')
target=${1:-"$HOME/.local/bin/agent-relay"}
mkdir -p "$(dirname "$target")"
{ printf '#!%s -I\n' "$python"; cat "$(dirname "$0")/agent_relay.py"; } > "$target.tmp"
chmod 755 "$target.tmp"
mv "$target.tmp" "$target"
echo "Installed $target ($("$target" --version))"
