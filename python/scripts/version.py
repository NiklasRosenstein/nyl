"""Derive package metadata from the Cargo workspace before building artifacts."""

import argparse
import re
import tomllib
from pathlib import Path


def python_version(cli_version: str) -> str:
    """Map strict SemVer releases and alpha.N/beta.N/rc.N prereleases to PEP 440."""
    number = r"(0|[1-9][0-9]*)"
    match = re.fullmatch(
        rf"{number}\.{number}\.{number}(?:-(alpha|beta|rc)\.{number})?", cli_version
    )
    if match is None:
        raise ValueError(f"Unsupported CLI version: {cli_version!r}")
    major, minor, patch, stage, serial = match.groups()
    mapped_minor = f"2{minor}" if major == "0" else minor
    stages = {"alpha": "a", "beta": "b", "rc": "rc"}
    suffix = f"{stages[stage]}{serial}" if stage else ""
    return f"{major}.{mapped_minor}.{patch}{suffix}"


def main() -> None:
    """Stamp the source metadata, verifying a supplied native release tag."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", help="Require this v-prefixed tag to match the workspace version")
    args = parser.parse_args()
    project = Path(__file__).resolve().parents[1]
    workspace = tomllib.loads((project.parent / "Cargo.toml").read_text())
    version = workspace["workspace"]["package"]["version"]
    if args.tag is not None and args.tag != f"v{version}":
        parser.error(f"Tag {args.tag!r} does not match workspace version {version!r}")
    derived = python_version(version)
    (project / "src/nyl/_version.py").write_text(f'__version__ = "{derived}"\n')
    print(derived)


if __name__ == "__main__":
    main()
