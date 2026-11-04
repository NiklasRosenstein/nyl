import os
import shutil
import subprocess
import sys
import tarfile
import zipfile
from email.parser import BytesParser
from pathlib import Path


def run(*args, cwd, env=None):
    return subprocess.run(args, cwd=cwd, env=env, check=True, capture_output=True, timeout=120)


def test_wheel_install_and_independent_sdist_build(tmp_path):
    project = Path(__file__).resolve().parents[1]
    source = tmp_path / "source"
    shutil.copytree(
        project, source, ignore=shutil.ignore_patterns(".venv", "dist", "__pycache__", ".*cache")
    )
    run(sys.executable, "-m", "build", "--no-isolation", cwd=source)
    wheel = next((source / "dist").glob("*.whl"))
    sdist = next((source / "dist").glob("*.tar.gz"))
    with zipfile.ZipFile(wheel) as archive:
        metadata = BytesParser().parsebytes(
            archive.read(
                next(name for name in archive.namelist() if name.endswith(".dist-info/METADATA"))
            )
        )
        assert "nyl/py.typed" in archive.namelist()
    with tarfile.open(sdist) as archive:
        archive.extractall(tmp_path / "extracted", filter="data")
    extracted = next((tmp_path / "extracted").iterdir())
    sdist_metadata = BytesParser().parsebytes((extracted / "PKG-INFO").read_bytes())
    assert sdist_metadata["Version"] == metadata["Version"]
    # No CLI, Cargo, repository metadata or version script is available to this build.
    env = {**os.environ, "PATH": str(tmp_path / "empty-path")}
    run(sys.executable, "-m", "build", "--wheel", "--no-isolation", cwd=extracted, env=env)
    rebuilt = next((extracted / "dist").glob("*.whl"))
    installed = tmp_path / "installed"
    run(
        sys.executable,
        "-m",
        "pip",
        "install",
        "--no-index",
        "--no-deps",
        "--target",
        str(installed),
        str(rebuilt),
        cwd=tmp_path,
        env=env,
    )
    code = (
        "import sys; sys.path.insert(0, sys.argv[1]); "
        "import nyl; from pathlib import Path; "
        "assert Path(nyl.__file__).is_relative_to(sys.argv[1]); "
        "from nyl import NylClient; from importlib.metadata import version; "
        "assert version('nyl') == sys.argv[2]; "
        "assert NylClient('.').timeout == 120"
    )
    run(sys.executable, "-c", code, str(installed), metadata["Version"], cwd=tmp_path, env=env)
