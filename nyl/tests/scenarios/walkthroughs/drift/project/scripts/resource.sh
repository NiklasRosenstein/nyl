#!/bin/sh
# Stands in for a resource outside Nyl: a file under $SCENARIO_TMP named after
# the unit. `apply` writes the expected content; `verify` exits 0 while the
# file still holds it and 2 (drift) once something else changed it.
set -eu
file="$SCENARIO_TMP/resources/$NYL_UNIT"
case "$1" in
  apply) mkdir -p "$(dirname "$file")"; printf 'applied\n' > "$file" ;;
  verify) test "$(cat "$file" 2>/dev/null)" = applied || exit 2 ;;
esac
