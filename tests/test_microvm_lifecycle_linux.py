"""Linux process-ownership tests; these do not launch or stand in for a VM."""
import importlib.util
import os
import select
import socket
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest

pytestmark = pytest.mark.linux_only
if sys.platform != "linux":
    pytest.skip("requires Linux pidfds and PR_SET_PDEATHSIG", allow_module_level=True)

SCRIPT = Path(__file__).parents[1] / "scripts/reach_microvm_broker.py"
SPEC = importlib.util.spec_from_file_location("reach_microvm_lifecycle", SCRIPT)
assert SPEC and SPEC.loader
broker = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = broker
SPEC.loader.exec_module(broker)


def test_child_survives_request_thread_but_not_broker_death():
    with tempfile.TemporaryDirectory(prefix="rvm-life-", dir="/tmp") as directory:
        endpoint = Path(directory) / "child.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.settimeout(10)
        listener.bind(str(endpoint))
        listener.listen(1)
        child = (
            "import socket,sys; connection=socket.socket(socket.AF_UNIX); "
            "connection.connect(sys.argv[1]); connection.sendall(b'ready'); "
            "data=connection.recv(4); connection.sendall(data); connection.recv(1)"
        )
        controller = f"""
import importlib.util,os,sys,threading
from pathlib import Path
from types import SimpleNamespace
spec=importlib.util.spec_from_file_location('owned_broker', {str(SCRIPT)!r})
module=importlib.util.module_from_spec(spec)
sys.modules[spec.name]=module
spec.loader.exec_module(module)
manager=module.Broker(SimpleNamespace())
command=[sys.executable, {str(SCRIPT)!r}, '--firecracker-bootstrap', str(os.getpid()), str(module._proc_starttime(os.getpid())), sys.executable, '-c', {child!r}, {str(endpoint)!r}]
results=[]
with open(os.devnull,'wb') as log:
    def request():
        results.append(manager._spawner.submit(module._spawn_owned, command, Path({directory!r}), log).result())
    thread=threading.Thread(target=request)
    thread.start()
    thread.join()
    if not results:
        raise RuntimeError('request failed to spawn child')
    print('request-finished',flush=True)
    sys.stdin.buffer.read()
"""
        owner = subprocess.Popen(
            [sys.executable, "-c", controller], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env={"HOME": directory, "PATH": "/usr/bin:/bin"},
        )
        connection = None
        try:
            connection, _ = listener.accept()
            connection.settimeout(5)
            assert connection.recv(5) == b"ready"
            assert owner.stdout is not None
            assert select.select([owner.stdout], [], [], 5)[0]
            assert owner.stdout.readline() == b"request-finished\n"
            connection.sendall(b"ping")
            assert connection.recv(4) == b"ping"
            owner.kill()
            owner.wait(timeout=5)
            assert connection.recv(1) == b""
        finally:
            if connection is not None:
                connection.close()
            listener.close()
            if owner.poll() is None:
                owner.kill()
            owner.communicate(timeout=5)


def test_dead_pidfd_cannot_signal_another_numeric_process_identity(tmp_path):
    with open(os.devnull, "wb") as log:
        expired, descriptor = broker._spawn_owned(
            [sys.executable, "-c", "pass"], tmp_path, log,
        )
        expired.wait(timeout=5)
        replacement = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdout.buffer.write(sys.stdin.buffer.read(1)); sys.stdout.flush()"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
            env={"HOME": str(tmp_path), "PATH": "/usr/bin:/bin"},
        )
        try:
            # The stale integer now points at another real process; the retained
            # kernel handle must remain bound to the already-exited child.
            expired.pid = replacement.pid
            broker._terminate_owned(expired, descriptor)
            stdout, _ = replacement.communicate(b"x", timeout=5)
            assert replacement.returncode == 0 and stdout == b"x"
        finally:
            os.close(descriptor)
            if replacement.poll() is None:
                replacement.kill()
            replacement.communicate(timeout=5)
