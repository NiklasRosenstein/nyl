#!/usr/bin/env bash
# Check that a Git host supports what Nyl's orchestration state needs:
# custom refs under refs/nyl/, and atomic pushes that update a branch and a
# custom ref together under per-ref leases, all or nothing.
#
# Usage: scripts/check-state-refs.sh <repository URL> [coordination prefix]
#
# The URL must be writable with your usual git credentials. The script works
# only on refs it creates, named with a random suffix, and deletes them again.
# The default coordination prefix is refs/nyl/; pass refs/heads/nyl-run/ to
# check the fallback for hosts that reject custom refs. It prints one line per
# check and exits non-zero when any check fails.
set -euo pipefail

url=${1:?usage: $0 <repository URL> [coordination prefix]}
prefix=${2:-refs/nyl/}
# Never print credentials embedded in the URL.
display=$(printf '%s' "$url" | sed -E 's#://[^@/]+@#://***@#')
suffix=$(date +%s)-$$
environment="nyl-check-$suffix"
branch="refs/heads/nyl-check/$suffix/desired"
lease="${prefix}${environment}/lease"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

git() { command git -C "$work" -c user.name=nyl-check -c user.email=nyl-check@example.invalid -c push.negotiate=false "$@"; }
commit() { git commit-tree "$(git write-tree)" -m "$1" ${2:+-p "$2"}; }
remote_ref() { command git ls-remote "$url" "$1" | cut -f1; }

failures=0
check() {
  local name=$1; shift
  if "$@" >"$work/.log" 2>&1; then echo "PASS  $name"; else echo "FAIL  $name"; sed -E 's#://[^@/]+@#://***@#; s/^/      /' "$work/.log"; failures=$((failures + 1)); fi
}

git init --quiet --bare .
state1=$(commit "state 1")
lease1=$(commit "lease 1")

create() {
  git push --porcelain --atomic "--force-with-lease=$branch:" "--force-with-lease=$lease:" "$url" "$state1:$branch" "$lease1:$lease"
  [ "$(remote_ref "$branch")" = "$state1" ] && [ "$(remote_ref "$lease")" = "$lease1" ]
}
check "create a branch and $lease in one atomic push" create

state2=$(commit "state 2" "$state1")
lease2=$(commit "lease 2" "$lease1")
stale() {
  # The lease expectation is wrong: neither ref may move.
  ! git push --porcelain --atomic "--force-with-lease=$branch:$state1" "--force-with-lease=$lease:$state1" "$url" "$state2:$branch" "$lease2:$lease"
  [ "$(remote_ref "$branch")" = "$state1" ] && [ "$(remote_ref "$lease")" = "$lease1" ]
}
check "reject the whole atomic push when one lease is stale" stale

advance() {
  git push --porcelain --atomic "--force-with-lease=$branch:$state1" "--force-with-lease=$lease:$lease1" "$url" "$state2:$branch" "$lease2:$lease"
  [ "$(remote_ref "$branch")" = "$state2" ] && [ "$(remote_ref "$lease")" = "$lease2" ]
}
check "advance both refs together under matching leases" advance

listed() { command git ls-remote "$url" "${prefix}${environment}/*" | grep -q "$lease"; }
check "list custom refs by pattern" listed

delete() {
  git push --porcelain --atomic "--force-with-lease=$lease:$lease2" "--force-with-lease=$branch:$state2" "$url" ":$lease" ":$branch"
  [ -z "$(remote_ref "$lease")" ] && [ -z "$(remote_ref "$branch")" ]
}
check "delete both refs under leases" delete

if [ "$failures" -gt 0 ]; then
  echo "$failures check(s) failed for $display with coordination prefix $prefix" >&2
  exit 1
fi
echo "All checks passed for $display with coordination prefix $prefix"
