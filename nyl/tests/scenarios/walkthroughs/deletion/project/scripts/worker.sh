#!/bin/sh
# Fails unless the cache it was given exists.
set -eu
cache=$(sed -n 's/.*"cache": *"\([^"]*\)".*/\1/p' "$NYL_INPUTS")
test -d "$cache"
