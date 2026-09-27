#!/bin/sh
# Creates or removes a directory standing in for a cache, and reports its path.
set -eu
value() { sed -n "s/.*\"$1\": *\"\([^\"]*\)\".*/\1/p" "$NYL_INPUTS"; }
dir="$(value dataDir)/cache"
case "$1" in
  create) mkdir -p "$dir"; printf '{"endpoint": "%s"}\n' "$dir" > "$NYL_OUTPUTS" ;;
  remove) rm -rf "$dir" ;;
esac
