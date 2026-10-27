#!/bin/sh
# Fails unless it was given the producer's release.
set -eu
release=$(sed -n 's/.*"release": *"\([^"]*\)".*/\1/p' "$NYL_INPUTS")
test -n "$release"
