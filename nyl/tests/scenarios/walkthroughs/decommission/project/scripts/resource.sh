#!/bin/sh
# Creates or removes a file standing in for the unit's resource beneath
# $SCENARIO_TMP, and reports its path.
set -eu
file="$SCENARIO_TMP/resources/$NYL_ENVIRONMENT/$NYL_UNIT"
case "$1" in
  create)
    mkdir -p "$(dirname "$file")"
    printf 'created\n' > "$file"
    if [ -n "${NYL_OUTPUTS:-}" ] && [ "${2:-}" = endpoint ]; then
      printf '{"endpoint": "%s"}\n' "$file" > "$NYL_OUTPUTS"
    fi ;;
  remove) rm -f "$file" ;;
esac
