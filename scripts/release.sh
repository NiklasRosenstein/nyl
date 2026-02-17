#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/release.sh <version> [--tag <tag>]

Examples:
  scripts/release.sh 0.2.1
  scripts/release.sh 0.2.1 --tag nyl/0.2.1

Behavior:
  1. Verifies git working tree is clean
  2. Updates version in nyl/Cargo.toml and Cargo.lock
  3. Commits the version bump
  4. Creates a git tag (default: v<version>)
  5. Pushes current branch and the tag to origin
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || "$#" -lt 1 ]]; then
  usage
  exit 0
fi

version="$1"
shift

tag="v${version}"
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --tag)
      if [[ "$#" -lt 2 ]]; then
        echo "error: --tag requires a value" >&2
        exit 1
      fi
      tag="$2"
      shift 2
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: version must look like semver (e.g. 0.2.1 or 0.2.1-rc.1)" >&2
  exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: git working tree is not clean" >&2
  git status --short
  exit 1
fi

branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$branch" == "HEAD" ]]; then
  echo "error: detached HEAD; checkout a branch first" >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  echo "error: tag already exists locally: ${tag}" >&2
  exit 1
fi

if git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1; then
  echo "error: tag already exists on origin: ${tag}" >&2
  exit 1
fi

tmp_file="$(mktemp)"

# Update nyl/Cargo.toml package version
awk -v v="$version" '
BEGIN { in_pkg = 0; updated = 0 }
{
  if ($0 == "[package]") {
    in_pkg = 1
    print
    next
  }
  if (in_pkg && $0 ~ /^version = "/ && !updated) {
    print "version = \"" v "\""
    updated = 1
    in_pkg = 0
    next
  }
  print
}
END {
  if (!updated) {
    print "error: failed to update version in nyl/Cargo.toml" > "/dev/stderr"
    exit 1
  }
}
' nyl/Cargo.toml > "${tmp_file}"
mv "${tmp_file}" nyl/Cargo.toml

# Update Cargo.lock root package entry for nyl
tmp_file="$(mktemp)"
awk -v v="$version" '
BEGIN { in_pkg = 0; is_nyl = 0; updated = 0 }
{
  if ($0 == "[[package]]") {
    in_pkg = 1
    is_nyl = 0
    print
    next
  }
  if (in_pkg && $0 == "name = \"nyl\"") {
    is_nyl = 1
    print
    next
  }
  if (in_pkg && is_nyl && $0 ~ /^version = "/ && !updated) {
    print "version = \"" v "\""
    updated = 1
    in_pkg = 0
    is_nyl = 0
    next
  }
  print
}
END {
  if (!updated) {
    print "error: failed to update version in Cargo.lock" > "/dev/stderr"
    exit 1
  }
}
' Cargo.lock > "${tmp_file}"
mv "${tmp_file}" Cargo.lock

git add nyl/Cargo.toml Cargo.lock
git commit -m "chore(release): bump nyl to ${version}"
git tag "${tag}"
git push origin "${branch}"
git push origin "${tag}"

echo "released ${version} on branch ${branch} with tag ${tag}"
