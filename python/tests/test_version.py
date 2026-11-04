import importlib.util
import subprocess
import sys
import tomllib
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/version.py"
spec = importlib.util.spec_from_file_location("version", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


@pytest.mark.parametrize(
    "cli,expected",
    [
        ("0.5.9", "0.25.9"),
        ("0.6.0", "0.26.0"),
        ("0.10.3", "0.210.3"),
        ("1.0.0", "1.0.0"),
        ("1.2.3", "1.2.3"),
        ("2.1.0", "2.1.0"),
        ("0.5.9-alpha.0", "0.25.9a0"),
        ("0.10.3-beta.2", "0.210.3b2"),
        ("1.2.3-rc.1", "1.2.3rc1"),
    ],
)
def test_python_version(cli, expected):
    assert module.python_version(cli) == expected


@pytest.mark.parametrize(
    "version",
    [
        "v0.5.9",
        "0.5",
        "01.2.3",
        "0.05.9",
        "1.2.03",
        "1.2.3-dev.1",
        "1.2.3-rc1",
        "1.2.3-rc.01",
        "1.2.3+local",
        "1.2.3-rc.1+build",
        "1.2.3\n",
    ],
)
def test_unsupported_version(version):
    with pytest.raises(ValueError, match="Unsupported CLI version"):
        module.python_version(version)


def test_stamped_metadata_matches_workspace():
    root = SCRIPT.parents[2]
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    version_file = (root / "python/src/nyl/_version.py").read_text()
    derived = module.python_version(workspace["workspace"]["package"]["version"])
    assert version_file == f'__version__ = "{derived}"\n'


def test_stamp_checks_native_tag_before_writing(tmp_path):
    project = tmp_path / "python"
    script = project / "scripts/version.py"
    script.parent.mkdir(parents=True)
    script.write_text(SCRIPT.read_text())
    metadata = project / "src/nyl/_version.py"
    metadata.parent.mkdir(parents=True)
    (tmp_path / "Cargo.toml").write_text('[workspace.package]\nversion = "0.10.3-rc.1"\n')
    subprocess.run([sys.executable, str(script), "--tag", "v0.10.3-rc.1"], check=True, timeout=10)
    assert metadata.read_text() == '__version__ = "0.210.3rc1"\n'
    result = subprocess.run(
        [sys.executable, str(script), "--tag", "v0.10.4"],
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert result.returncode != 0
    assert "does not match workspace version" in result.stderr
    assert metadata.read_text() == '__version__ = "0.210.3rc1"\n'
