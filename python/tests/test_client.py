import os
import signal
import subprocess
import sys
import time
import traceback
from contextlib import suppress
from pathlib import Path
from unittest.mock import MagicMock, Mock

import pytest

from nyl import (
    ExecutableNotFoundError,
    InvalidOutputError,
    NylClient,
    RenderError,
    RenderTimeoutError,
)


def stub_process(monkeypatch, *, stdout=b"", stderr=b"", exit_code=0):
    process = MagicMock(returncode=exit_code)
    process.__enter__.return_value = process
    process.communicate.return_value = stdout, stderr
    factory = Mock(return_value=process)
    monkeypatch.setattr(subprocess, "Popen", factory)
    return process, factory


def test_invocation_isolated_and_flags(tmp_path, monkeypatch):
    cwd, environment = Path.cwd(), os.environ.copy()
    _, factory = stub_process(monkeypatch, stderr=b"diagnostic")
    client = NylClient(tmp_path, executable="custom-nyl", timeout=17)
    assert len(client.render("-file with spaces.yaml", target="dev", offline=True)) == 0
    client.render(tmp_path / "absolute.yaml", allow_secret_inputs=True)
    first, second = factory.call_args_list
    assert first.args[0] == [
        "custom-nyl",
        "render",
        "--target",
        "dev",
        "--offline",
        "--",
        "-file with spaces.yaml",
    ]
    assert second.args[0] == [
        "custom-nyl",
        "render",
        "--allow-secret-inputs",
        "--",
        str(tmp_path / "absolute.yaml"),
    ]
    assert first.kwargs == {
        "shell": False,
        "cwd": tmp_path,
        "stdout": subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "start_new_session": os.name != "nt",
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


@pytest.mark.parametrize("newline", ["\n", "\r\n"])
def test_timeout_retains_diagnostics_without_stdout(tmp_path, newline):
    # Python accepts this filename as its script argument after argv[1] = render.
    (tmp_path / "render").write_text(
        "import sys, time\n"
        f"sys.stderr.reconfigure(newline={newline!r})\n"
        "print('SECRET-STDOUT', flush=True)\n"
        "print('slow render', file=sys.stderr, flush=True)\n"
        "time.sleep(30)\n"
    )
    with pytest.raises(RenderTimeoutError) as failure:
        NylClient(tmp_path, executable=sys.executable, timeout=0.5).render("web.yaml")
    assert failure.value.diagnostics.splitlines() == ["slow render"]
    assert failure.value.exit_code is None
    assert "SECRET-STDOUT" not in "".join(traceback.format_exception(failure.value))


def test_timeout_stops_descendant_processes(tmp_path):
    child = tmp_path / "child.py"
    child.write_text(
        "import os, pathlib, sys, time\n"
        "root = pathlib.Path(__file__).parent\n"
        "(root/'child.pid').write_text(str(os.getpid()))\n"
        "print('child-ready', file=sys.stderr, flush=True)\n"
        "for tick in range(600):\n"
        "    (root/'heartbeat').write_text(str(tick))\n"
        "    time.sleep(0.05)\n",
        encoding="utf-8",
    )
    (tmp_path / "render").write_text(
        "import subprocess, sys\n"
        f"subprocess.run([sys.executable, {str(child)!r}], check=True, timeout=35)\n",
        encoding="utf-8",
    )
    try:
        with pytest.raises(RenderTimeoutError) as failure:
            NylClient(tmp_path, executable=sys.executable, timeout=2).render("web.yaml")
        assert "child-ready" in failure.value.diagnostics
        heartbeat = (tmp_path / "heartbeat").read_text()
        time.sleep(0.3)
        assert (tmp_path / "heartbeat").read_text() == heartbeat
    finally:
        if (tmp_path / "child.pid").exists():
            with suppress(OSError):
                os.kill(int((tmp_path / "child.pid").read_text()), signal.SIGTERM)


def test_interrupt_terminates_the_process_tree(tmp_path, monkeypatch):
    process, _ = stub_process(monkeypatch)
    process.communicate.side_effect = [KeyboardInterrupt, (b"", b"")]
    terminate = Mock()
    monkeypatch.setattr("nyl.client._terminate_tree", terminate)
    with pytest.raises(KeyboardInterrupt):
        NylClient(tmp_path).render("web.yaml")
    terminate.assert_called_once_with(process)


@pytest.mark.parametrize("exit_code", [0, 7])
def test_errors_do_not_expose_rendered_bodies(tmp_path, monkeypatch, exit_code):
    stub_process(monkeypatch, stdout=b"SECRET-STDOUT: [", stderr=b"diagnostic", exit_code=exit_code)
    with pytest.raises(RenderError if exit_code else InvalidOutputError) as failure:
        NylClient(tmp_path).render("web.yaml")
    assert "SECRET-STDOUT" not in "".join(traceback.format_exception(failure.value))
