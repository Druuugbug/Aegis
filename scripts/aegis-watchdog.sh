#!/usr/bin/env bash
# aegis-watchdog — external crash/restart supervisor for checkpoint-resume.
#
# Runs `aegis chat </dev/null`, which resumes any *active* resumable tasks
# (registered via the agent's `task register` tool) and then exits at EOF.
# A completed task (status=done) is skipped, so this is a cheap no-op once the
# work is finished. Use flock so overlapping cron ticks never run twice.
#
# Install (resume every 15 minutes):
#   crontab -e
#   */15 * * * * /path/to/aegis/scripts/aegis-watchdog.sh >> "$HOME/.aegis/watchdog.log" 2>&1
#
# Override the binary with AEGIS_BIN=/path/to/aegis if it is not on PATH.
set -euo pipefail

mkdir -p "${HOME}/.aegis"
LOCK="${HOME}/.aegis/watchdog.lock"
exec 9>"$LOCK"
if ! flock -n 9; then
    echo "[$(date -Is)] watchdog: another run in progress; skipping"
    exit 0
fi

AEGIS_BIN="${AEGIS_BIN:-aegis}"
echo "[$(date -Is)] watchdog: resuming active tasks…"
"$AEGIS_BIN" chat </dev/null
echo "[$(date -Is)] watchdog: done"
