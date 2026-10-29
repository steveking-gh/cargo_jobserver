#!/usr/bin/env bash
# restart_jobserver.sh: (re)load the cargo jobserver launchd agent.
# Safe to run whether or not the agent is currently loaded.
set -euo pipefail

label=com.kings.cargo-jobserver
plist=~/Library/LaunchAgents/$label.plist
domain=gui/$(id -u)

# bootout fails if the agent is not loaded (first install); that is fine.
launchctl bootout "$domain/$label" 2>/dev/null || true
launchctl bootstrap "$domain" "$plist"

# Give the binary a moment to start and log its startup line.
sleep 1
tail -2 ~/Library/Logs/cargo-jobserver.log
