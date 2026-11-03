import os
import subprocess
import sys
import traceback
from pathlib import Path

import pytest

from nyl import (
    ExecutableNotFoundError,
    InvalidOutputError,
    NylClient,
    RenderError,
    RenderTimeoutError,
)


def test_invocation_isolated_and_flags(tmp_path, monkeypatch):
    cwd, environment = Path.cwd(), os.environ.copy()
    calls = []

    def run(command, **kwargs):
        calls.append((command, kwargs))
        return subprocess.CompletedProcess(command, 0, b"", b"diagnostic")

    monkeypatch.setattr(subprocess, "run", run)
    client = NylClient(tmp_path, executable="custom-nyl", timeout=17)
    assert len(client.render("-file with spaces.yaml", target="dev", offline=True)) == 0
    client.render(tmp_path / "absolute.yaml", allow_secret_inputs=True)
    assert calls[0][0] == [
        "custom-nyl",
        "render",
        "--target",
        "dev",
        "--offline",
        "--",
        "-file with spaces.yaml",
    ]
    assert calls[1][0] == [
        "custom-nyl",
        "render",
        "--allow-secret-inputs",
        "--",
        str(tmp_path / "absolute.yaml"),
    ]
    assert calls[0][1] == {
        "shell": False,
        "cwd": tmp_path,
        "capture_output": True,
        "timeout": 17,
        "check": False,
    }
    assert Path.cwd() == cwd
    assert os.environ == environment


@pytest.mark.parametrize("timeout", [0, -1, float("inf"), float("-inf"), float("nan")])
def test_timeout_must_be_positive_and_finite(tmp_path, timeout):
    with pytest.raises(ValueError, match="positive and finite"):
        NylClient(tmp_path, timeout=timeout)


def test_missing_executable_and_project(tmp_path):
    client = NylClient(tmp_path, executable=tmp_path / "no-such-nyl")
    with pytest.raises(ExecutableNotFoundError, match="install the Nyl CLI separately") as failure:
        client.render("web.yaml")
    assert failure.value.command[0] == str(tmp_path / "no-such-nyl")
    assert failure.value.exit_code is None
    with pytest.raises(RenderError, match="Project directory does not exist"):
        NylClient(tmp_path / "absent").render("web.yaml")


def test_nonzero_does_not_return_partial_stdout(tmp_path, cli):
    (tmp_path / "nyl.toml").write_text("")
    with pytest.raises(RenderError) as failure:
        NylClient(tmp_path, executable=cli).render("missing.yaml")
    assert failure.value.exit_code != 0
    assert failure.value.diagnostics
    assert failure.value.command == (str(cli), "render", "--", "missing.yaml")


def test_timeout_retains_diagnostics_without_stdout(tmp_path):
    # Python accepts this filename as its script argument after argv[1] = render.
    (tmp_path / "render").write_text(
        "import sys, time\n"
        "print('SECRET-STDOUT', flush=True)\n"
        "print('slow render', file=sys.stderr, flush=True)\n"
        "time.sleep(30)\n"
    )
    with pytest.raises(RenderTimeoutError) as failure:
        NylClient(tmp_path, executable=sys.executable, timeout=0.5).render("web.yaml")
    assert failure.value.diagnostics == "slow render\n"
    assert failure.value.exit_code is None
    assert "SECRET-STDOUT" not in "".join(traceback.format_exception(failure.value))


@pytest.mark.parametrize("exit_code", [0, 7])
def test_errors_do_not_expose_rendered_bodies(tmp_path, monkeypatch, exit_code):
    def run(command, **kwargs):
        return subprocess.CompletedProcess(command, exit_code, b"SECRET-STDOUT: [", b"diagnostic")

    monkeypatch.setattr(subprocess, "run", run)
    with pytest.raises(RenderError if exit_code else InvalidOutputError) as failure:
        NylClient(tmp_path).render("web.yaml")
    assert "SECRET-STDOUT" not in "".join(traceback.format_exception(failure.value))
