#!/usr/bin/env bash
# aegis-supervise — instant-restart supervisor for resumable tasks.
#
# Unlike the cron watchdog (coarse, e.g. every 15 min), this stays alive and
# restarts aegis within ~$AEGIS_BACKOFF seconds of ANY exit or crash, until no
# active resumable task remains. Run it under nohup / tmux / systemd:
#
#   nohup scripts/aegis-supervise.sh >> "$HOME/.aegis/supervise.log" 2>&1 &
#
# Env:
#   AEGIS_BIN      aegis binary (default: aegis on PATH)
#   AEGIS_BACKOFF  seconds between restarts (default: 2)
set -uo pipefail

AEGIS_BIN="${AEGIS_BIN:-aegis}"
BACKOFF="${AEGIS_BACKOFF:-2}"
TASKS_DIR="${HOME}/.aegis/tasks/persistent"

have_active() {
    grep -lq '"status": "active"' "$TASKS_DIR"/*.json 2>/dev/null
}

if ! have_active; then
    echo "[$(date -Is)] supervise: no active resumable tasks; nothing to do."
    exit 0
fi

while have_active; do
    echo "[$(date -Is)] supervise: (re)starting aegis resume…"
    # `</dev/null` → resume runs, then exits at EOF. A crash/non-zero exit just
    # loops again (the restart_count guard inside aegis stops runaway loops).
    "$AEGIS_BIN" chat </dev/null || echo "[$(date -Is)] supervise: aegis exited $?"
    sleep "$BACKOFF"
done
echo "[$(date -Is)] supervise: all tasks complete/stopped; exiting."
