#!/bin/sh
# Stands in for a DNS provider: records the address it was given under
# $SCENARIO_TMP.
set -eu
mkdir -p "$SCENARIO_TMP/dns"
cp "$NYL_INPUTS" "$SCENARIO_TMP/dns/web.json"
