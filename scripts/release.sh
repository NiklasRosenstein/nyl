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
  2. Updates the workspace version in Cargo.toml and Cargo.lock
  3. For a final release, renames .changelog/_unreleased.toml to
     .changelog/<version>.toml with today's release-date; a prerelease
     (for example 0.2.1-rc.1) leaves the unreleased entries in place
  4. Commits the version bump and changelog in one commit
  5. Creates a git tag (default: v<version>)
  6. Pushes current branch and the tag to origin
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

# A final release moves the unreleased changelog entries into the version's
# file. A prerelease keeps them for the final release.
unreleased_changelog=".changelog/_unreleased.toml"
release_changelog=""
if [[ "$version" != *-* ]]; then
  release_changelog=".changelog/${version%%+*}.toml"
  if [[ ! -f "$unreleased_changelog" ]]; then
    echo "error: ${unreleased_changelog} is missing; add the release's changelog entries first" >&2
    exit 1
  fi
  if [[ -e "$release_changelog" ]]; then
    echo "error: ${release_changelog} already exists" >&2
    exit 1
  fi
fi

# Restore the manifests and the changelog if any step before the commit
# fails, so the clean-tree check does not block a re-run.
restore_worktree() {
  git reset -q -- Cargo.toml Cargo.lock .changelog
  git checkout -- Cargo.toml Cargo.lock
  if [[ -n "$release_changelog" ]]; then
    rm -f "$release_changelog"
    git checkout -- "$unreleased_changelog"
  fi
}
trap restore_worktree ERR

tmp_file="$(mktemp)"

# Update the shared workspace package version in Cargo.toml
awk -v v="$version" '
BEGIN { in_pkg = 0; updated = 0 }
{
  if ($0 == "[workspace.package]") {
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
    print "error: failed to update the workspace version in Cargo.toml" > "/dev/stderr"
    exit 1
  }
}
' Cargo.toml > "${tmp_file}"
mv "${tmp_file}" Cargo.toml

# Refresh the Cargo.lock entries of every workspace member from the bumped
# manifest, then confirm the lockfile is consistent with it.
cargo update --workspace
cargo metadata --locked --no-deps --format-version 1 >/dev/null

git add Cargo.toml Cargo.lock

if [[ -n "$release_changelog" ]]; then
  {
    printf 'release-date = "%s"\n\n' "$(date -u +%Y-%m-%d)"
    cat "$unreleased_changelog"
  } > "$release_changelog"
  git rm -q "$unreleased_changelog"
  git add "$release_changelog"
fi

git commit -m "chore(release): bump nyl to ${version}"
trap - ERR
git tag "${tag}"
git push origin "${branch}"
git push origin "${tag}"

echo "released ${version} on branch ${branch} with tag ${tag}"
