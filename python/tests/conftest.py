import os
from pathlib import Path

import pytest


@pytest.fixture(scope="session")
def cli() -> Path:
    # The calling task builds this path from the checkout before starting pytest.
    checkout_binary = Path(__file__).resolve().parents[2] / "target/debug/nyl"
    if os.name == "nt":
        checkout_binary = checkout_binary.with_suffix(".exe")
    path = Path(os.environ.get("NYL_TEST_EXECUTABLE", str(checkout_binary))).resolve()
    if not path.is_file():
        pytest.fail(f"Build the checkout CLI first (mise run python-test): {path}")
    return path
