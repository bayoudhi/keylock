#!/usr/bin/env bash
# Fake long-running migration for trying out keylock.
#
# It behaves like a fragile real script:
#   - Space pauses it, and ANY key after that aborts the whole run.
#   - Ctrl+C kills it.
#
# Usage: ./examples/migrate.sh [batches]   (default 120, one per second)
#
# Without keylock:   ./examples/migrate.sh        -> press Space, then any key: aborted
# With keylock:      keylock run --locked -- ./examples/migrate.sh
#                    -> Space, keys and Ctrl+C do nothing until you unlock

set -u

batches=${1:-120}

trap 'printf "\n\033[31mInterrupted with Ctrl+C: migration aborted at batch %d/%d\033[0m\n" "$i" "$batches"; exit 130' INT

echo "Migrating $batches batches (Space pauses; any key while paused aborts)"
[ -n "${KEYLOCK_NAME:-}" ] && echo "Running under keylock as session: $KEYLOCK_NAME"

i=0
while [ "$i" -lt "$batches" ]; do
  i=$((i + 1))
  printf "\rbatch %d/%d migrated" "$i" "$batches"

  # Wait about a second for a key; no key means keep going.
  if IFS= read -rsn1 -t 1 key; then
    if [ "$key" = " " ]; then
      printf "\n\033[33mPaused. Press any key to ABORT the migration...\033[0m"
      IFS= read -rsn1 _
      printf "\n\033[31mAborted by keypress at batch %d/%d\033[0m\n" "$i" "$batches"
      exit 1
    fi
  fi
done

printf "\n\033[32mMigration finished: %d/%d batches\033[0m\n" "$batches" "$batches"
