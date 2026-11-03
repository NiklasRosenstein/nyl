import json
import os
import subprocess
from pathlib import Path

import pytest


@pytest.fixture(scope="session")
def cli() -> Path:
    executable = os.environ.get("NYL_TEST_EXECUTABLE")
    if executable is None:
        build = subprocess.run(
            ["cargo", "build", "--locked", "-p", "nyl", "--message-format=json-render-diagnostics"],
            cwd=Path(__file__).resolve().parents[1],
            stdout=subprocess.PIPE,
            encoding="utf-8",
            check=True,
            timeout=1200,
        )
        for line in build.stdout.splitlines():
            artifact = json.loads(line)
            if (
                artifact.get("reason") == "compiler-artifact"
                and artifact["target"]["name"] == "nyl"
                and "bin" in artifact["target"]["kind"]
                and artifact.get("executable")
            ):
                executable = artifact["executable"]
                break
        else:
            pytest.fail("Cargo did not report a nyl executable artifact")
    path = Path(executable).resolve()
    if not path.is_file():
        pytest.fail(f"Build the checkout CLI first (mise run python-test): {path}")
    return path
