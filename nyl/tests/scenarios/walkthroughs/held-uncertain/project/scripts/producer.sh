#!/bin/sh
# Publishes the release it was given, such as a schema version, as its output.
set -eu
release=$(sed -n 's/.*"release": *"\([^"]*\)".*/\1/p' "$NYL_INPUTS")
printf '{"release": "%s"}\n' "$release" > "$NYL_OUTPUTS"
