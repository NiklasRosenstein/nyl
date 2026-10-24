#!/usr/bin/env bash
# Run scripts/check-state-refs.sh against a throwaway Forgejo server.
#
# Usage: scripts/check-state-refs-forgejo.sh <forgejo binary>
#
# Starts Forgejo with SQLite in a temporary directory on a free local port,
# creates a user and a repository, checks both coordination prefixes, and
# stops the server again. CI runs this so Forgejo support is proven on every
# change; GitHub and GitLab are checked by hand with check-state-refs.sh.
set -euo pipefail

forgejo=$(realpath "${1:?usage: $0 <forgejo binary>}")
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
password="nyl-check-$RANDOM$RANDOM"
export FORGEJO_WORK_DIR="$work"

cat >"$work/app.ini" <<INI
APP_NAME = nyl-check
RUN_MODE = prod
# Containers and CI sandboxes may run as root; this server is throwaway.
I_AM_BEING_UNSAFE_RUNNING_AS_ROOT = true
WORK_PATH = $work
[server]
HTTP_ADDR = 127.0.0.1
HTTP_PORT = $port
ROOT_URL = http://127.0.0.1:$port/
DISABLE_SSH = true
OFFLINE_MODE = true
[database]
DB_TYPE = sqlite3
PATH = $work/forgejo.db
[repository]
ROOT = $work/repositories
[security]
INSTALL_LOCK = true
[service]
DISABLE_REGISTRATION = true
[log]
ROOT_PATH = $work/log
INI

"$forgejo" --config "$work/app.ini" migrate >/dev/null
"$forgejo" --config "$work/app.ini" admin user create --admin --username nyl --password "$password" \
  --email nyl@example.invalid --must-change-password=false >/dev/null
"$forgejo" --config "$work/app.ini" web >"$work/web.log" 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null; wait "$server" 2>/dev/null; rm -rf "$work"' EXIT

for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$port/api/v1/version" >/dev/null && break
  sleep 1
done
curl -sf -u "nyl:$password" -H 'Content-Type: application/json' \
  -d '{"name": "state", "auto_init": true, "default_branch": "main"}' \
  "http://127.0.0.1:$port/api/v1/user/repos" >/dev/null

url="http://nyl:$password@127.0.0.1:$port/nyl/state.git"
"$forgejo" --version
"$here/check-state-refs.sh" "$url" refs/nyl/
"$here/check-state-refs.sh" "$url" refs/heads/nyl-run/
