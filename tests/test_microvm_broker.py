"""Focused confinement tests for the real microVM broker/guest boundary.

These tests never launch Firecracker or a guest.  Main runs the broader Python
and live nested-guest validation after the concurrent implementation settles.
"""
from __future__ import annotations

import base64
import fcntl
import http.client
import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest

_ROOT = Path(__file__).parents[1]

def _load(name: str, relative: str):
    spec = importlib.util.spec_from_file_location(name, _ROOT / relative)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


broker = _load("reach_microvm_broker", "scripts/reach_microvm_broker.py")
guest = _load("reach_microvm_guest", "scripts/reach_microvm_guest.py")

@pytest.fixture
def unix_tmp_path():
    # macOS's default pytest path can exceed sockaddr_un.sun_path.
    with tempfile.TemporaryDirectory(prefix="rvm-", dir="/tmp") as directory:
        yield Path(directory)


def test_valid_rpc_runtime_failure_is_not_reported_as_malformed_input(unix_tmp_path):
    def fail_dispatch(method, params):
        raise OSError(28, "private runtime failure detail")

    owner = SimpleNamespace(
        config=SimpleNamespace(images={}, max_memory_mib=1536),
        dispatch=fail_dispatch,
    )
    server = broker._UnixHTTPServer(str(unix_tmp_path / "rpc.sock"), owner)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    with patch.object(broker, "_peer_uid_allowed", return_value=True):
        thread.start()
        try:
            with socket.socket(socket.AF_UNIX) as stream:
                stream.settimeout(5)
                stream.connect(str(unix_tmp_path / "rpc.sock"))
                body = json.dumps({"method": "list", "params": {}}).encode()
                stream.sendall(
                    f"POST /v1/rpc HTTP/1.1\r\nHost: localhost\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body
                )
                response = http.client.HTTPResponse(stream)
                response.begin()
                payload = json.load(response)
                assert response.status == 503
                assert payload["error"]["code"] == "unavailable"
                assert "private runtime failure detail" not in json.dumps(payload)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


def _image() -> broker.Image:
    return broker.Image("gold", Path("/private/kernel"), Path("/private/rootfs"), "a" * 64, "b" * 64)


def _config(**changes):
    value = {
        "name": "screen-0",
        "image": "gold",
        "resolution": {"width": 1280, "height": 720},
        "shm_size": 512 * 1024 * 1024,
        "ports": {"vnc": 5900, "novnc": 6080, "health": 8400, "extra": []},
        "screens": 1,
        "profile": None,
        "workspace": None,
        "memory": None,
        "restart_unless_stopped": False,
        "vnc_password": None,
        "allow_exec": True,
        "writable_workspace": False,
    }
    value.update(changes)
    return value


def test_unregistered_image_is_rejected_before_create_side_effects():
    request = {"method": "create", "params": {"config": _config(image="request-supplied-host-path")}}
    with pytest.raises(broker.BrokerError, match="registered"):
        broker.validate_rpc_request(request, {"gold": _image()}, 1536)


def test_host_workspace_profile_and_extra_port_are_rejected():
    images = {"gold": _image()}
    for changed in (
        {"workspace": "/private/host"},
        {"profile": {"name": "personal", "host_path": "/private/profile", "container_path": "/home/sandbox/profile"}},
        {"ports": {"vnc": 5900, "novnc": 6080, "health": 8400, "extra": [[127, 9222]]}},
        {"restart_unless_stopped": True},
    ):
        with pytest.raises(broker.BrokerError):
            broker.validate_rpc_request({"method": "create", "params": {"config": _config(**changed)}}, images, 1536)


def test_non_512_mib_shm_is_rejected_before_allocation():
    request = {"method": "create", "params": {"config": _config(shm_size=256 * 1024 * 1024)}}
    with pytest.raises(broker.BrokerError, match="512"):
        broker.validate_rpc_request(request, {"gold": _image()}, 1536)


def test_target_requires_full_uuid_or_exact_name():
    with pytest.raises(broker.BrokerError):
        broker.validate_rpc_request({"method": "incarnation", "params": {"target": "screen/name"}}, {}, 1536)

def test_stale_uuid_does_not_resolve_to_replacement_name():
    stale_id = "deadbeef-abcd-1abc-8def-0123456789ab"
    replacement_id = "feedface-cafe-49ab-9abc-fedcba987654"
    config = SimpleNamespace(images={}, max_memory_mib=1536)
    manager = broker.Broker.__new__(broker.Broker)
    manager.config = config
    manager._lock = threading.RLock()
    manager._guests = {
        replacement_id: SimpleNamespace(
            name=stale_id, runtime_id=replacement_id, status="running"
        ),
    }
    with pytest.raises(broker.BrokerError) as failure:
        manager._resolve(stale_id)
    assert failure.value.status == 404
    with pytest.raises(broker.BrokerError) as failure:
        manager._resolve("00000000-0000-4000-8000")
    assert failure.value.status == 404
    with pytest.raises(broker.BrokerError, match="invalid sandbox name"):
        broker.validate_rpc_request(
            {"method": "create", "params": {"config": _config(name=stale_id)}},
            {"gold": _image()},
            1536,
        )
def test_trusted_exec_input_runs_non_code_guest_maintenance_and_rejects_invalid_stale_targets(unix_tmp_path):
    endpoint = unix_tmp_path / "vsock.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.settimeout(5)
    listener.bind(str(endpoint))
    listener.listen(2)

    service = guest.GuestService("/private/reach-supervisor")
    service.configured = True
    service.screens = 1

    class FixtureGuestService:
        def dispatch(self, request):
            if request.get("op") != "exec":
                return service.dispatch(request)
            command = guest._validate_command(request["command"])
            input_data = guest._decode_stdin(request["stdin_base64"])
            env = os.environ.copy()
            env.update({"HOME": str(unix_tmp_path), "PATH": "/usr/bin:/bin"})
            return guest._bounded_exec(command, input_data, request["timeout_seconds"], env), None

    def guest_endpoint():
        for _ in range(2):
            connection, _ = listener.accept()
            connection.settimeout(5)
            with connection:
                assert connection.recv(64) == b"CONNECT 1024\n"
                connection.sendall(b"OK\n")
                guest._serve_connection(
                    FixtureGuestService(), connection, (guest.HOST_CID, 1024)
                )

    executor = ThreadPoolExecutor(max_workers=1)
    endpoint_future = executor.submit(guest_endpoint)
    runtime_id = "24289793-e334-48a7-ba61-5d9c9f041aad"
    guest_state = SimpleNamespace(
        runtime_id=runtime_id,
        name="screen-0",
        status="running",
        config=_config(allow_exec=False),
        process=SimpleNamespace(poll=lambda: None),
        vsock_path=endpoint,
    )
    manager = broker.Broker.__new__(broker.Broker)
    manager.config = SimpleNamespace(exec_timeout_seconds=5)
    manager._lock = threading.RLock()
    manager._stopping = False
    manager._guests = {runtime_id: guest_state}
    marker = unix_tmp_path / "maintenance.marker"
    request = {
        "target": runtime_id,
        "command": [
            sys.executable,
            "-c",
            f"import pathlib,sys; pathlib.Path({str(marker)!r}).write_text(sys.stdin.read())",
        ],
        "input_base64": base64.b64encode(b"maintenance").decode(),
    }
    try:
        with pytest.raises(broker.BrokerError) as failure:
            broker.validate_rpc_request(
                {"method": "exec_input", "params": {**request, "target": "screen/name"}},
                {"gold": _image()},
                1536,
            )
        assert failure.value.status == 400
        stale_id = "deadbeef-abcd-4abc-8def-0123456789ab"
        with pytest.raises(broker.BrokerError) as failure:
            manager.dispatch("exec_input", {**request, "target": stale_id})
        assert failure.value.status == 404
        health = manager._guest_call(guest_state, {"op": "health"})
        assert set(health) == {"boot_id", "kernel"}
        result = manager.dispatch("exec_input", request)
        assert result == {"exit_code": 0, "stdout": "", "stderr": ""}
        assert marker.read_text() == "maintenance"
    finally:
        listener.close()
        executor.shutdown(wait=True)
    endpoint_future.result()



def _frame(value: dict) -> bytes:
    payload = json.dumps(value, separators=(",", ":")).encode()
    return len(payload).to_bytes(4, "big") + payload


def _recv_bytes(sock: socket.socket, length: int) -> bytes:
    result = bytearray()
    while len(result) < length:
        chunk = sock.recv(length - len(result))
        if not chunk:
            raise EOFError("peer closed before completing the frame")
        result.extend(chunk)
    return bytes(result)

def test_vsock_request_rejects_guest_error_envelope(unix_tmp_path):
    endpoint = unix_tmp_path / "vsock.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.settimeout(5)
    listener.bind(str(endpoint))
    listener.listen(1)

    def respond():
        connection, _ = listener.accept()
        with connection:
            assert connection.recv(64) == b"CONNECT 1024\n"
            connection.sendall(b"OK\n")
            length = int.from_bytes(_recv_bytes(connection, 4), "big")
            _recv_bytes(connection, length)
            connection.sendall(_frame({"error": {"code": "unavailable", "message": "private detail"}}))

    worker = threading.Thread(target=respond)
    worker.start()
    try:
        with broker._Vsock(endpoint, 5) as channel, pytest.raises(broker.BrokerError) as failure:
            channel.request({"op": "health"})
        assert failure.value.code == "unavailable"
        assert "private detail" not in str(failure.value)
    finally:
        listener.close()
        worker.join(timeout=5)


@pytest.mark.linux_only
@pytest.mark.skipif(
    sys.platform != "linux" or os.geteuid() == 0,
    reason="broker startup requires a non-root Linux pidfd environment",
)
def test_start_stop_interleaving_cleans_resources_before_serving(unix_tmp_path):
    config = broker.BrokerConfig(
        unix_tmp_path / "broker.sock", unix_tmp_path / "state",
        unix_tmp_path / "firecracker", {}, 1024, 1536, 1, 1.0,
    )
    manager = broker.Broker(config)
    constructor_entered = threading.Event()
    release_constructor = threading.Event()
    stop_started = threading.Event()

    class BlockingServer(broker._UnixHTTPServer):
        def __init__(self, path, owner):
            super().__init__(path, owner)
            constructor_entered.set()
            if not release_constructor.wait(5):
                self.server_close()
                raise TimeoutError("server construction was not released")

    def start():
        try:
            manager.start()
        except broker.BrokerError as error:
            return error
        return None

    def stop():
        stop_started.set()
        manager.stop()

    try:
        with (
            ThreadPoolExecutor(max_workers=2) as workers,
            patch.object(broker, "_UnixHTTPServer", BlockingServer),
        ):
            start_result = workers.submit(start)
            try:
                assert constructor_entered.wait(5)
                stop_result = workers.submit(stop)
                assert stop_started.wait(5)
            finally:
                release_constructor.set()
            error = start_result.result(timeout=5)
            stop_result.result(timeout=5)
            assert error is None or error.code == "unavailable"
    finally:
        manager.stop()
    assert not config.socket.exists()
    for lock_path in (
        config.state_dir / "broker.lock",
        config.socket.with_name(config.socket.name + ".lock"),
    ):
        with lock_path.open("r+b") as lock_file:
            fcntl.flock(lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)


def test_viewer_forwarding_bridges_bytes_after_decoding_guest_envelope(unix_tmp_path):
    endpoint = unix_tmp_path / "vsock.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.settimeout(5)
    listener.bind(str(endpoint))
    listener.listen(1)
    received_request: list[dict] = []

    def guest_endpoint():
        connection, _ = listener.accept()
        connection.settimeout(5)
        with connection:
            request_line = connection.recv(64)
            assert request_line == b"CONNECT 1024\n"
            connection.sendall(b"OK\n")
            frame_length = int.from_bytes(_recv_bytes(connection, 4), "big")
            frame = _recv_bytes(connection, frame_length)
            received_request.append(json.loads(frame))
            connection.sendall(_frame({"result": {"connected": True}}))
            assert connection.recv(5) == b"hello"
            connection.sendall(b"world")

    endpoint_thread = threading.Thread(target=guest_endpoint)
    endpoint_thread.start()
    viewer, viewer_peer = socket.socketpair()
    viewer_peer.settimeout(5)
    manager = broker.Broker.__new__(broker.Broker)
    manager._stopping = False
    guest_runtime = SimpleNamespace(vsock_path=endpoint)
    forward_thread = threading.Thread(target=manager._forward_one, args=(guest_runtime, viewer, 6080))
    forward_thread.start()
    try:
        viewer_peer.sendall(b"hello")
        assert viewer_peer.recv(5) == b"world"
        assert received_request == [{"op": "connect", "port": 6080}]
    finally:
        viewer_peer.close()
        forward_thread.join(timeout=2)
        endpoint_thread.join(timeout=2)
        listener.close()




def test_guest_rejects_non_host_cid_before_dispatch():
    service = guest.GuestService("/private/reach-supervisor")
    connection = SimpleNamespace(close=lambda: None)
    # The connection handler checks the peer tuple before reading a request.
    with patch.object(guest, "_read_frame", side_effect=AssertionError("request must not be read")):
        guest._serve_connection(service, connection, (7, 1024))


def test_guest_connect_restricts_to_configured_novnc_range():
    service = guest.GuestService("/private/reach-supervisor")
    service.configured = True
    service.screens = 1
    with pytest.raises(guest.GuestError, match="port"):
        service.connect({"op": "connect", "port": 5900})


def test_guest_exec_rejects_malformed_command_before_process_spawn():
    service = guest.GuestService("/private/reach-supervisor")
    service.configured = True
    with (
        patch.object(guest.subprocess, "Popen", side_effect=AssertionError("must validate first")),
        pytest.raises(guest.GuestError),
    ):
        service.execute({"command": [], "stdin_base64": base64.b64encode(b"").decode(), "timeout_seconds": 1})


def test_guest_timeout_closes_live_descendants_after_leader_exit(unix_tmp_path):
    endpoint = unix_tmp_path / "child.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.settimeout(10)
    listener.bind(str(endpoint))
    listener.listen(1)
    child = (
        "import socket,sys; connection=socket.socket(socket.AF_UNIX); "
        "connection.connect(sys.argv[1]); connection.sendall(b'ready'); "
        "connection.recv(1)"
    )
    launcher = (
        "import subprocess,sys; "
        f"subprocess.Popen([sys.executable, '-c', {child!r}, {str(endpoint)!r}])"
    )
    results = []

    def execute():
        try:
            guest._bounded_exec(
                [sys.executable, "-c", launcher], b"", 5,
                {"HOME": str(unix_tmp_path), "PATH": "/usr/bin:/bin"},
            )
        except guest.GuestError as error:
            results.append(error)

    worker = threading.Thread(target=execute, daemon=True)
    worker.start()
    connection = None
    try:
        connection, _ = listener.accept()
        connection.settimeout(5)
        assert _recv_bytes(connection, 5) == b"ready"
        worker.join(timeout=10)
        assert not worker.is_alive()
        assert len(results) == 1 and results[0].code == "timeout"
        assert connection.recv(1) == b""
    finally:
        if connection is not None:
            connection.close()
        listener.close()
        worker.join(timeout=10)



def test_guest_returns_result_after_output_pipes_close_before_leader_exit(unix_tmp_path):
    marker = unix_tmp_path / "marker"
    script = (
        "import os,time; os.close(1); os.close(2); time.sleep(1); "
        f"open({str(marker)!r}, 'w').write('done')"
    )
    result = guest._bounded_exec(
        [sys.executable, "-c", script], b"", 5,
        {"HOME": str(unix_tmp_path), "PATH": "/usr/bin:/bin"},
    )
    assert result["exit_code"] == 0
    assert marker.read_text() == "done"


def test_destroy_timeout_is_structured_and_retains_pidfd_owned_guest():
    runtime_id = "00000000-0000-4000-8000-000000000001"
    guest_state = SimpleNamespace(
        runtime_id=runtime_id, status="running", cleaned=False,
        process=object(), pidfd=77, monitor=None,
    )
    manager = broker.Broker.__new__(broker.Broker)
    manager._lock = threading.RLock()
    manager._stopping = False
    manager._guests = {runtime_id: guest_state}
    with (
        patch.object(
            broker, "_terminate_owned",
            side_effect=subprocess.TimeoutExpired("wait", 2),
        ),
        pytest.raises(broker.BrokerError) as failure,
    ):
        manager.destroy(runtime_id)
    assert failure.value.status == 503
    assert manager._guests[runtime_id] is guest_state
    assert guest_state.status == "stopping"
    assert guest_state.pidfd == 77


def test_monitor_reclaims_guest_after_synchronous_destroy_timeout():
    runtime_id = "00000000-0000-4000-8000-000000000001"
    guest_state = SimpleNamespace(
        runtime_id=runtime_id, status="stopping", cleaned=False,
        process=SimpleNamespace(wait=lambda: 0),
    )
    manager = broker.Broker.__new__(broker.Broker)
    manager._lock = threading.RLock()
    manager._guests = {runtime_id: guest_state}

    def cleanup(value):
        value.cleaned = True

    manager._cleanup_guest = cleanup
    manager._monitor(guest_state)
    assert guest_state.cleaned is True
    assert runtime_id not in manager._guests
def test_failed_start_cleanup_remains_owned_until_stop_reclaims_same_guest(tmp_path):
    runtime_id = "00000000-0000-4000-8000-000000000001"
    process = SimpleNamespace(poll=lambda: None)
    image = broker.Image(
        "gold", tmp_path / "kernel", tmp_path / "rootfs", "a" * 64, "b" * 64,
    )
    image.rootfs.write_bytes(b"rootfs")
    manager = broker.Broker.__new__(broker.Broker)
    manager._lock = threading.RLock()
    manager._lifecycle_lock = threading.Lock()
    manager._lifecycle_condition = threading.Condition(manager._lock)
    manager._startup_state = "idle"
    manager._stopping = False
    manager._guests = {}
    manager._server = None
    manager._serving = False
    manager._socket_identity = None
    manager._lock_fds = []
    manager.config = SimpleNamespace(
        images={"gold": image},
        memory_mib=512,
        max_memory_mib=1536,
        max_guests=1,
        state_dir=tmp_path,
        socket=tmp_path / "reach-microvm-broker-test.sock",
        firecracker=Path("/private/firecracker"),
        exec_timeout_seconds=10,
    )
    submitted_logs = []

    def submit(function, command, directory, log_file):
        submitted_logs.append(log_file)
        return SimpleNamespace(result=lambda: (process, 77))

    manager._spawner = SimpleNamespace(submit=submit, shutdown=lambda wait: None)

    cleanup_calls = []

    def destroy(value):
        cleanup_calls.append((value, value.process, value.pidfd))
        if len(cleanup_calls) == 1:
            raise OSError("cleanup failed")
        value.cleaned = True
        value.log_file.close()

    manager._destroy_guest = destroy
    try:
        with (
            patch.object(broker.uuid, "uuid4", return_value=uuid.UUID(runtime_id)),
            patch.object(broker, "_parse_sandbox_config", return_value=_config()),
            patch.object(broker, "_trusted_artifact"),
            patch.object(broker, "_proc_starttime", return_value=1),
            patch.object(manager, "_wait_ready", side_effect=broker.BrokerError("guest_failed", "not ready")),
            pytest.raises(OSError, match="cleanup failed"),
        ):
            broker.Broker.create(manager, _config())
    finally:
        for log_file in submitted_logs:
            log_file.close()

    guest_state = manager._guests[runtime_id]
    assert guest_state is cleanup_calls[0][0]
    assert guest_state.status == "stopping"
    assert cleanup_calls[0][1:] == (process, 77)

    manager.stop()

    assert cleanup_calls[1] == (guest_state, process, 77)
    assert guest_state.cleaned is True
    assert runtime_id not in manager._guests
