#!/usr/bin/env python3
"""AF_VSOCK service installed as the init process in a Reach guest image.

The service has no host filesystem or TCP control surface.  Firecracker's host
CID (2) is the only accepted peer, and every control message is an 8 MiB,
length-prefixed JSON object.  Guest execution is performed as ``sandbox`` and
viewer forwarding can target only the guest-loopback noVNC listeners.
"""
from __future__ import annotations

import argparse
import base64
import binascii
import errno
import json
import os
import pwd
import select
import selectors
import signal
import socket
import subprocess
import threading
import time
from collections.abc import Mapping
from pathlib import Path
from typing import Any

MAX_FRAME = 8 * 1024 * 1024
MAX_INPUT = 4 * 1024 * 1024
MAX_OUTPUT = 4 * 1024 * 1024
MAX_ARGS = 128
MAX_ARG_BYTES = 64 * 1024
HOST_CID = 2
VSOCK_PORT = 1024
FRAME_TIMEOUT_SECONDS = 30.0
EXEC_RESPONSE_MARGIN_SECONDS = 5.0


class GuestError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


def _recv_exact(sock: socket.socket, length: int, deadline: float | None = None) -> bytes:
    result = bytearray()
    while len(result) < length:
        if deadline is not None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise GuestError("protocol", "frame read exceeded its deadline")
            sock.settimeout(remaining)
        chunk = sock.recv(min(65536, length - len(result)))
        if not chunk:
            raise GuestError("protocol", "peer closed the connection")
        result.extend(chunk)
    return bytes(result)


def _read_frame(sock: socket.socket) -> dict[str, Any]:
    deadline = time.monotonic() + FRAME_TIMEOUT_SECONDS
    raw_length = _recv_exact(sock, 4, deadline)
    length = int.from_bytes(raw_length, "big")
    if length > MAX_FRAME:
        raise GuestError("frame_too_large", "request frame is too large")
    try:
        value = json.loads(_recv_exact(sock, length, deadline))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise GuestError("protocol", "request frame is malformed") from exc
    if not isinstance(value, dict):
        raise GuestError("protocol", "request must be an object")
    return value


def _write_frame(sock: socket.socket, value: Mapping[str, Any], timeout: float | None = None) -> None:
    payload = json.dumps(value, separators=(",", ":")).encode()
    if len(payload) > MAX_FRAME:
        raise GuestError("protocol", "response frame is too large")
    previous_timeout = sock.gettimeout()
    if timeout is not None:
        sock.settimeout(timeout)
    try:
        sock.sendall(len(payload).to_bytes(4, "big") + payload)
    finally:
        if timeout is not None:
            sock.settimeout(previous_timeout)


def _validate_command(value: Any) -> list[str]:
    if not isinstance(value, list) or not value or len(value) > MAX_ARGS:
        raise GuestError("invalid_request", "command must be a bounded non-empty array")
    if any(not isinstance(arg, str) or not arg or "\x00" in arg or len(arg.encode()) > MAX_ARG_BYTES for arg in value):
        raise GuestError("invalid_request", "command contains an invalid argument")
    return value


def _decode_stdin(value: Any) -> bytes:
    if not isinstance(value, str) or len(value) > ((MAX_INPUT + 2) // 3) * 4:
        raise GuestError("invalid_request", "stdin_base64 is invalid")
    try:
        decoded = base64.b64decode(value.encode("ascii"), validate=True)
    except (UnicodeEncodeError, binascii.Error) as exc:
        raise GuestError("invalid_request", "stdin_base64 is invalid") from exc
    if len(decoded) > MAX_INPUT:
        raise GuestError("invalid_request", "stdin is too large")
    return decoded


def _valid_dimensions(value: Any, label: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise GuestError("invalid_request", f"{label} is invalid")
    return value


def _kill_group(process: subprocess.Popen[bytes], process_group: int | None = None) -> None:
    # Keep the process-group identity captured at spawn time.  Do not call
    # poll() first: it reaps the leader while descendants may still own the
    # pipes, and a later PID/group lookup could target a reused process.
    pgid = process_group if process_group is not None else process.pid
    try:
        os.killpg(pgid, signal.SIGKILL)
    except OSError as exc:
        if exc.errno != errno.ESRCH:
            raise
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        # The original group remains the only owned target.  Never fall back
        # to process.kill(), which could signal a reused PID.
        try:
            os.killpg(pgid, signal.SIGKILL)
        except OSError as exc:
            if exc.errno != errno.ESRCH:
                raise
        process.wait(timeout=2)


def _bounded_exec(command: list[str], stdin_data: bytes, timeout: float, env: Mapping[str, str]) -> dict[str, Any]:
    if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not 0.1 <= timeout <= 120:
        raise GuestError("invalid_request", "timeout_seconds is invalid")
    try:
        process = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            cwd=env.get("HOME", "/home/sandbox"), env=dict(env), start_new_session=True, close_fds=True,
        )
        process_group = os.getpgid(process.pid)
    except (OSError, ValueError) as exc:
        raise GuestError("exec_failed", "guest command could not be started") from exc
    assert process.stdin is not None and process.stdout is not None and process.stderr is not None
    streams: dict[int, tuple[str, Any]] = {
        process.stdout.fileno(): ("stdout", process.stdout),
        process.stderr.fileno(): ("stderr", process.stderr),
    }
    selector = selectors.DefaultSelector()
    for fd, (_name, stream) in streams.items():
        os.set_blocking(fd, False)
        selector.register(fd, selectors.EVENT_READ, stream)
    input_offset = 0
    stdin_fd = process.stdin.fileno()
    os.set_blocking(stdin_fd, False)
    selector.register(stdin_fd, selectors.EVENT_WRITE, process.stdin)
    outputs = {"stdout": bytearray(), "stderr": bytearray()}
    deadline = time.monotonic() + float(timeout)
    timed_out = False
    too_large = False
    try:
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                timed_out = True
                break
            for key, mask in selector.select(min(0.1, remaining)):
                fd = key.fd
                if fd == stdin_fd and mask & selectors.EVENT_WRITE:
                    if input_offset >= len(stdin_data):
                        selector.unregister(fd)
                        process.stdin.close()
                    else:
                        try:
                            written = os.write(fd, stdin_data[input_offset:input_offset + 65536])
                            input_offset += written
                        except BlockingIOError:
                            pass
                        except BrokenPipeError:
                            selector.unregister(fd)
                            process.stdin.close()
                elif mask & selectors.EVENT_READ:
                    try:
                        chunk = os.read(fd, 65536)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(fd)
                        streams[fd][1].close()
                        continue
                    stream_name = streams[fd][0]
                    outputs[stream_name].extend(chunk)
                    if len(outputs["stdout"]) + len(outputs["stderr"]) > MAX_OUTPUT:
                        too_large = True
                        break
            if too_large:
                break
    finally:
        selector.close()
    if timed_out or too_large:
        _kill_group(process, process_group)
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                stream.close()
        if timed_out:
            raise GuestError("timeout", "guest command exceeded its deadline")
        raise GuestError("output_limit", "guest command output exceeded its limit")
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        _kill_group(process, process_group)
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                stream.close()
        raise GuestError("timeout", "guest command exceeded its deadline")
    try:
        process.wait(timeout=remaining)
    except subprocess.TimeoutExpired:
        _kill_group(process, process_group)
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                stream.close()
        raise GuestError("timeout", "guest command exceeded its deadline")
    return {"exit_code": int(process.returncode), "stdout": outputs["stdout"].decode("utf-8", "replace"), "stderr": outputs["stderr"].decode("utf-8", "replace")}


class GuestService:
    def __init__(self, supervisor: str = "/usr/local/bin/reach-supervisor"):
        self.supervisor = supervisor
        self.configured = False
        self.screens = 0
        self.supervisor_process: subprocess.Popen[bytes] | None = None
        self.boot_id = self._read_boot_id()

    @staticmethod
    def _read_boot_id() -> str:
        try:
            value = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="ascii").strip()
        except OSError:
            value = "unknown"
        return value[:128]

    def dispatch(self, request: Mapping[str, Any]) -> tuple[dict[str, Any], socket.socket | None]:
        if not isinstance(request.get("op"), str):
            raise GuestError("invalid_request", "request operation is invalid")
        op = request["op"]
        if op == "health":
            if set(request) != {"op"}:
                raise GuestError("invalid_request", "health takes no parameters")
            return {"boot_id": self.boot_id, "kernel": os.uname().release}, None
        if op == "configure":
            if set(request) != {"op", "width", "height", "screens", "vnc_password"}:
                raise GuestError("invalid_request", "configure parameters are invalid")
            return self.configure(request), None
        if op == "exec":
            if set(request) != {"op", "command", "stdin_base64", "timeout_seconds"}:
                raise GuestError("invalid_request", "exec parameters are invalid")
            return self.execute(request), None
        if op == "connect":
            if set(request) != {"op", "port"}:
                raise GuestError("invalid_request", "connect parameters are invalid")
            return self.connect(request)
        raise GuestError("invalid_request", "unknown operation")

    def configure(self, request: Mapping[str, Any]) -> dict[str, Any]:
        if self.configured:
            raise GuestError("conflict", "guest is already configured")
        width = _valid_dimensions(request["width"], "width", 320, 4096)
        height = _valid_dimensions(request["height"], "height", 240, 4096)
        screens = _valid_dimensions(request["screens"], "screens", 1, 8)
        password = request["vnc_password"]
        if password is not None and (not isinstance(password, str) or not password or len(password) > 4096 or any(ord(c) < 0x21 or ord(c) > 0x7e for c in password)):
            raise GuestError("invalid_request", "vnc_password is invalid")
        env = os.environ.copy()
        env.update({
            "DISPLAY_NUM": "99", "WIDTH": str(width), "HEIGHT": str(height),
            "REACH_SCREENS": str(screens), "REACH_SUPERVISOR_HOST": "127.0.0.1",
            "DISPLAY": ":99", "HOME": "/home/sandbox",
            "PATH": "/opt/reach-venv/bin:/usr/local/bin:/usr/bin:/bin",
            "PLAYWRIGHT_BROWSERS_PATH": "/opt/ms-playwright",
        })
        if password is None:
            env.pop("VNC_PASSWORD", None)
        else:
            env["VNC_PASSWORD"] = password
        try:
            process = subprocess.Popen([self.supervisor], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env, cwd="/home/sandbox", start_new_session=True, close_fds=True)
        except OSError as exc:
            raise GuestError("configure_failed", "reach-supervisor could not be started") from exc
        self.supervisor_process = process
        self.configured = True
        self.screens = screens
        return {"configured": True}

    def execute(self, request: Mapping[str, Any]) -> dict[str, Any]:
        command = _validate_command(request["command"])
        if not self.configured:
            raise GuestError("conflict", "guest is not configured")
        if self.supervisor_process is not None and self.supervisor_process.poll() is not None and command[0] == "curl":
            raise GuestError("unavailable", "reach-supervisor has exited")
        stdin_data = _decode_stdin(request["stdin_base64"])
        env = os.environ.copy()
        env.update({"PATH": "/opt/reach-venv/bin:/usr/local/bin:/usr/bin:/bin", "HOME": "/home/sandbox", "PLAYWRIGHT_BROWSERS_PATH": "/opt/ms-playwright"})
        return _bounded_exec(command, stdin_data, request["timeout_seconds"], env)

    def connect(self, request: Mapping[str, Any]) -> tuple[dict[str, Any], socket.socket | None]:
        if not self.configured:
            raise GuestError("conflict", "guest is not configured")
        port = _valid_dimensions(request["port"], "port", 6080, 6080 + self.screens - 1)
        try:
            stream = socket.create_connection(("127.0.0.1", port), timeout=5)
        except OSError as exc:
            raise GuestError("unavailable", "noVNC loopback stream is unavailable") from exc
        return {"connected": True}, stream


def _serve_connection(service: GuestService, connection: socket.socket, address: Any) -> None:
    stream: socket.socket | None = None
    try:
        if not isinstance(address, tuple) or not address or address[0] != HOST_CID:
            connection.close()
            return
        connection.settimeout(30)
        while True:
            request = _read_frame(connection)
            stream = None
            response_timeout = EXEC_RESPONSE_MARGIN_SECONDS if request.get("op") == "exec" else FRAME_TIMEOUT_SECONDS
            try:
                result, stream = service.dispatch(request)
                _write_frame(connection, {"result": result}, response_timeout)
            except GuestError as exc:
                _write_frame(connection, {"error": {"code": exc.code, "message": exc.message}}, response_timeout)
                if exc.code in {"protocol", "frame_too_large"}:
                    return
                continue
            if stream is not None:
                _bridge(connection, stream)
                return
    except (OSError, GuestError):
        return
    finally:
        if stream is not None:
            stream.close()
        connection.close()


def _bridge(left: socket.socket, right: socket.socket) -> None:
    left.settimeout(None)
    right.settimeout(None)
    sockets = [left, right]
    while sockets:
        readable, _, _ = select.select(sockets, [], [], 30)
        if not readable:
            return
        for source in readable:
            data = source.recv(65536)
            if not data:
                return
            (right if source is left else left).sendall(data)


def _drop_to_sandbox(user: str) -> None:
    account = pwd.getpwnam(user)
    if os.geteuid() == 0:
        os.initgroups(user, account.pw_gid)
        os.setgid(account.pw_gid)
        os.setuid(account.pw_uid)
    elif os.geteuid() != account.pw_uid:
        raise GuestError("invalid_config", "guest service is not running as sandbox")


def main() -> int:
    parser = argparse.ArgumentParser(description="Reach guest AF_VSOCK service")
    parser.add_argument("--supervisor", default="/usr/local/bin/reach-supervisor")
    parser.add_argument("--sandbox-user", default="sandbox")
    args = parser.parse_args()
    _drop_to_sandbox(args.sandbox_user)
    service = GuestService(args.supervisor)
    listener = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((socket.VMADDR_CID_ANY, VSOCK_PORT))
    listener.listen(8)
    try:
        while True:
            connection, address = listener.accept()
            threading.Thread(target=_serve_connection, args=(service, connection, address), daemon=True, name="reach-vsock-client").start()
    finally:
        listener.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
