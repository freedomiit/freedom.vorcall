#!/usr/bin/env bash
# Nightly logical backup of the Vorcall Postgres database.
#
# Runs ON THE HOST as the `ubuntu` user; installed by deploy/provision-host.sh
# as /usr/local/bin/vorcall-backup-db with this cron line
# (/etc/cron.d/vorcall-backup):
#
#   15 3 * * * <user> APP_DIR=<app-dir> /usr/local/bin/vorcall-backup-db >> <app-dir>/backups/cron.log 2>&1
#
# Writes $APP_DIR/backups/vorcall-<UTC stamp>.dump (pg_dump custom format),
# deletes dumps older than 14 days and appends one line per run to
# $APP_DIR/backups/backup.log.
#
# RESTORE (destructive — it drops and recreates the objects in the dump):
#
#   cd <app-dir>
#   set -a; . .env; set +a   # pg_restore's -U/-d below are expanded by this shell
#   docker compose -f docker-compose.prod.yml stop backend
#   docker compose -f docker-compose.prod.yml exec -T db \
#       pg_restore -U "$POSTGRES_USER" -d "$POSTGRES_DB" --clean --if-exists --no-owner \
#       < backups/<file>.dump
#   docker compose -f docker-compose.prod.yml start backend
#   curl -fsS localhost:5004/health
set -euo pipefail

APP_DIR="${APP_DIR:-/opt/vorcall}"
BACKUP_DIR="$APP_DIR/backups"
LOG="$BACKUP_DIR/backup.log"
KEEP_DAYS=14

umask 077
mkdir -p "$BACKUP_DIR"

stamp="$(date -u +%Y%m%dT%H%M%SZ)"

fail() {
    printf '%s FAILED: %s\n' "$stamp" "$1" >> "$LOG"
    echo "backup failed: $1" >&2
    exit 1
}

if [ ! -f "$APP_DIR/.env" ]; then
    fail "$APP_DIR/.env is missing"
fi
set -a
# shellcheck disable=SC1091
. "$APP_DIR/.env"
set +a

if [ -z "${POSTGRES_USER:-}" ] || [ -z "${POSTGRES_DB:-}" ]; then
    fail "POSTGRES_USER or POSTGRES_DB is not set in $APP_DIR/.env"
fi

out="$BACKUP_DIR/vorcall-$stamp.dump"
if ! docker compose -f "$APP_DIR/docker-compose.prod.yml" exec -T db \
        pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" -Fc > "$out.part"; then
    rm -f "$out.part"
    fail "pg_dump returned a non-zero status"
fi
if ! mv "$out.part" "$out"; then
    rm -f "$out.part"
    fail "could not move $out.part into place"
fi

find "$BACKUP_DIR" -name 'vorcall-*.dump' -mtime +$KEEP_DAYS -delete

size="$(stat -c %s "$out")"
printf '%s OK %s %s bytes\n' "$stamp" "$(basename "$out")" "$size" >> "$LOG"
echo "backup ok: $out ($size bytes)"
