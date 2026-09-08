#!/usr/bin/env python3
"""Non-root Firecracker broker for Reach microVM sandboxes.

The broker is deliberately a small, private control plane.  Its only public
transport is an owner-only Unix socket; guest commands and viewer streams are
carried over Firecracker's AF_VSOCK endpoint.  Request data never supplies a
host artifact path and every mutable operation is serialized under ``_lock``.
"""
from __future__ import annotations

import argparse
import base64
import binascii
import ctypes
import fcntl
import hashlib
import http.server
import json
import logging
import os
import re
import select
import shutil
import signal
import socket
import stat
import subprocess
import sys
import threading
import time
import uuid
from collections.abc import Mapping
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Self

MAX_FRAME = 8 * 1024 * 1024
MAX_COMMAND_ARGS = 128
MAX_COMMAND_ARG_BYTES = 64 * 1024
MAX_EXEC_INPUT = 4 * 1024 * 1024
MAX_EXEC_OUTPUT = 4 * 1024 * 1024
EXEC_TRANSPORT_MARGIN_SECONDS = 5.0
MAX_EXEC_TRANSPORT_SECONDS = 125.0
SHM_SIZE = 512 * 1024 * 1024
NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,62}$")
UUID_RE = re.compile(r"^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
LOGGER = logging.getLogger(__name__)


class BrokerError(Exception):
    def __init__(self, code: str, message: str, status: int = 400):
        super().__init__(message)
        self.code = code
        self.message = message
        self.status = status


@dataclass(frozen=True)
class Image:
    name: str
    kernel: Path
    rootfs: Path
    kernel_sha256: str
    rootfs_sha256: str


@dataclass(frozen=True)
class BrokerConfig:
    socket: Path
    state_dir: Path
    firecracker: Path
    images: dict[str, Image]
    memory_mib: int
    max_memory_mib: int
    max_guests: int
    exec_timeout_seconds: float

@dataclass
class Guest:
    name: str
    config: dict[str, Any]
    runtime_id: str
    image: Image
    work_dir: Path
    rootfs: Path
    vsock_path: Path
    process: subprocess.Popen[bytes]
    pidfd: int
    log_file: Any
    created_at: str
    guest_cid: int
    status: str = "starting"
    forwarders: list[tuple[socket.socket, threading.Thread]] = field(default_factory=list)
    monitor: threading.Thread | None = None
    cleaned: bool = False


_ALLOWED_CONFIG = {
    "name", "image", "resolution", "shm_size", "ports", "screens", "profile",
    "workspace", "memory", "restart_unless_stopped", "vnc_password", "allow_exec",
    "writable_workspace",
}
_ALLOWED_PORTS = {"vnc", "novnc", "health", "extra"}


def _strict_object(value: Any, label: str) -> Mapping[str, Any]:
    if not isinstance(value, dict):
        raise BrokerError("invalid_request", f"{label} must be an object")
    return value


def _strict_int(value: Any, label: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise BrokerError("invalid_request", f"{label} is outside its allowed range")
    return value


def _strict_bool(value: Any, label: str) -> bool:
    if not isinstance(value, bool):
        raise BrokerError("invalid_request", f"{label} must be boolean")
    return value


def _parse_sandbox_config(raw: Any, images: Mapping[str, Image], max_memory_mib: int) -> dict[str, Any]:
    obj = _strict_object(raw, "config")
    unknown = set(obj) - _ALLOWED_CONFIG
    missing = _ALLOWED_CONFIG - set(obj)
    if unknown or missing:
        raise BrokerError("invalid_request", "config fields do not match the sandbox schema")
    name = obj["name"]
    if not isinstance(name, str) or not NAME_RE.fullmatch(name) or UUID_RE.fullmatch(name):
        raise BrokerError("invalid_request", "invalid sandbox name")
    image = obj["image"]
    if not isinstance(image, str) or image not in images:
        raise BrokerError("invalid_request", "image is not registered")
    resolution = _strict_object(obj["resolution"], "resolution")
    if set(resolution) != {"width", "height"}:
        raise BrokerError("invalid_request", "resolution fields are invalid")
    width = _strict_int(resolution["width"], "resolution.width", 320, 4096)
    height = _strict_int(resolution["height"], "resolution.height", 240, 4096)
    ports = _strict_object(obj["ports"], "ports")
    if set(ports) != _ALLOWED_PORTS:
        raise BrokerError("invalid_request", "ports fields are invalid")
    # Firecracker guests have no host binds or arbitrary host-port publication.
    if ports["vnc"] != 5900 or ports["novnc"] != 6080 or ports["health"] != 8400:
        raise BrokerError("invalid_request", "only the guest's fixed private ports are supported")
    if ports["extra"] != []:
        raise BrokerError("unsupported_port", "extra port publication is not supported")
    screens = _strict_int(obj["screens"], "screens", 1, 8)
    shm_size = _strict_int(obj["shm_size"], "shm_size", 16 * 1024 * 1024, 4 * 1024 * 1024 * 1024)
    if shm_size != SHM_SIZE:
        raise BrokerError("unsupported_shm", "only a 512 MiB shared-memory mount is supported")
    profile = obj["profile"]
    if profile is not None:
        raise BrokerError("unsupported_profile", "host browser profiles are not available to microVMs")
    workspace = obj["workspace"]
    if workspace is not None:
        raise BrokerError("unsupported_workspace", "host workspace binds are not available to microVMs")
    memory = obj["memory"]
    if memory is not None:
        memory = _strict_int(memory, "memory", 128 * 1024 * 1024, max_memory_mib * 1024 * 1024)
    restart = _strict_bool(obj["restart_unless_stopped"], "restart_unless_stopped")
    if restart:
        raise BrokerError("unsupported_restart", "restart policy is not supported by the broker")
    allow_exec = _strict_bool(obj["allow_exec"], "allow_exec")
    writable = _strict_bool(obj["writable_workspace"], "writable_workspace")
    if writable:
        raise BrokerError("unsupported_workspace", "writable host workspaces are not available to microVMs")
    password = obj["vnc_password"]
    if password is not None and (not isinstance(password, str) or not password or len(password) > 4096 or any(ord(c) < 0x21 or ord(c) > 0x7e for c in password)):
        raise BrokerError("invalid_request", "invalid viewer password")
    return {
        "name": name, "image": image, "resolution": {"width": width, "height": height},
        "shm_size": shm_size, "ports": {"vnc": 5900, "novnc": 6080, "health": 8400, "extra": []},
        "screens": screens, "profile": None, "workspace": None, "memory": memory,
        "restart_unless_stopped": False, "vnc_password": password, "allow_exec": allow_exec,
        "writable_workspace": False,
    }


def _decode_input(value: Any) -> bytes:
    if not isinstance(value, str) or len(value) > ((MAX_EXEC_INPUT + 2) // 3) * 4:
        raise BrokerError("invalid_request", "input_base64 is invalid")
    try:
        decoded = base64.b64decode(value.encode("ascii"), validate=True)
    except (UnicodeEncodeError, binascii.Error) as exc:
        raise BrokerError("invalid_request", "input_base64 is invalid") from exc
    if len(decoded) > MAX_EXEC_INPUT:
        raise BrokerError("invalid_request", "input is too large")
    return decoded


def _validate_command(value: Any) -> list[str]:
    if not isinstance(value, list) or not value or len(value) > MAX_COMMAND_ARGS:
        raise BrokerError("invalid_request", "command must be a bounded non-empty array")
    if any(not isinstance(arg, str) or not arg or "\x00" in arg or len(arg.encode()) > MAX_COMMAND_ARG_BYTES for arg in value):
        raise BrokerError("invalid_request", "command contains an invalid argument")
    return value


def validate_rpc_request(raw: Any, images: Mapping[str, Image], max_memory_mib: int) -> tuple[str, dict[str, Any]]:
    obj = _strict_object(raw, "request")
    if set(obj) != {"method", "params"} or not isinstance(obj["method"], str):
        raise BrokerError("invalid_request", "request must contain method and params")
    method = obj["method"]
    params = _strict_object(obj["params"], "params")
    if method == "list":
        if params:
            raise BrokerError("invalid_request", "list takes no parameters")
    elif method == "create":
        if set(params) != {"config"}:
            raise BrokerError("invalid_request", "create parameters are invalid")
        _parse_sandbox_config(params["config"], images, max_memory_mib)
    elif method in {"destroy", "inspect_config", "incarnation"}:
        if set(params) != {"target"}:
            raise BrokerError("invalid_request", "target parameters are required")
        _validate_target(params["target"])
    elif method == "exec_input":
        if set(params) != {"target", "command", "input_base64"}:
            raise BrokerError("invalid_request", "exec_input parameters are invalid")
        _validate_target(params["target"])
        _validate_command(params["command"])
        _decode_input(params["input_base64"])
    else:
        raise BrokerError("invalid_request", "unknown RPC method")
    return method, dict(params)


def _validate_target(target: Any) -> str:
    if not isinstance(target, str) or not target or len(target) > 64:
        raise BrokerError("invalid_request", "invalid target")
    if UUID_RE.fullmatch(target):
        return target
    if NAME_RE.fullmatch(target):
        return target
    raise BrokerError("invalid_request", "target must be an exact name or UUID")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _trusted_artifact(path: Path, expected: str, label: str) -> None:
    try:
        metadata = path.lstat()
    except OSError as exc:
        raise BrokerError("invalid_config", f"{label} is unavailable", 503) from exc
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise BrokerError("invalid_config", f"{label} must be a regular file", 503)
    if _sha256(path) != expected:
        raise BrokerError("invalid_config", f"{label} digest mismatch", 503)




def load_config(path: str | os.PathLike[str]) -> BrokerConfig:
    config_path = Path(path)
    try:
        raw = json.loads(config_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise BrokerError("invalid_config", "broker configuration is unreadable", 503) from exc
    obj = _strict_object(raw, "broker configuration")
    required = {"socket", "state_dir", "firecracker", "images", "memory_mib", "max_memory_mib", "max_guests", "exec_timeout_seconds"}
    if set(obj) != required:
        raise BrokerError("invalid_config", "broker configuration fields are invalid", 503)
    paths: dict[str, Path] = {}
    for key in ("socket", "state_dir", "firecracker"):
        value = obj[key]
        if not isinstance(value, str) or not value.startswith("/") or "\x00" in value:
            raise BrokerError("invalid_config", f"{key} must be an absolute local path", 503)
        paths[key] = Path(value)
    try:
        firecracker_metadata = paths["firecracker"].lstat()
    except OSError as exc:
        raise BrokerError("invalid_config", "firecracker executable is unavailable", 503) from exc
    if stat.S_ISLNK(firecracker_metadata.st_mode) or not stat.S_ISREG(firecracker_metadata.st_mode) or not os.access(paths["firecracker"], os.X_OK):
        raise BrokerError("invalid_config", "firecracker must be a regular executable", 503)
    memory_mib = _strict_int(obj["memory_mib"], "memory_mib", 128, 1024 * 1024)
    max_memory_mib = _strict_int(obj["max_memory_mib"], "max_memory_mib", memory_mib, 1024 * 1024)
    max_guests = _strict_int(obj["max_guests"], "max_guests", 1, 1024)
    timeout = obj["exec_timeout_seconds"]
    if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not 0.1 <= timeout <= 120:
        raise BrokerError("invalid_config", "exec_timeout_seconds is invalid", 503)
    images_raw = _strict_object(obj["images"], "images")
    if not images_raw:
        raise BrokerError("invalid_config", "at least one image must be registered", 503)
    images: dict[str, Image] = {}
    for name, image_raw in images_raw.items():
        if not isinstance(name, str) or not NAME_RE.fullmatch(name):
            raise BrokerError("invalid_config", "invalid registered image name", 503)
        image_obj = _strict_object(image_raw, "image")
        if set(image_obj) != {"kernel", "rootfs", "kernel_sha256", "rootfs_sha256"}:
            raise BrokerError("invalid_config", "registered image fields are invalid", 503)
        image_paths: list[Path] = []
        for key in ("kernel", "rootfs"):
            value = image_obj[key]
            if not isinstance(value, str) or not value.startswith("/") or "\x00" in value:
                raise BrokerError("invalid_config", "image artifact paths must be absolute", 503)
            image_paths.append(Path(value))
        hashes = [image_obj["kernel_sha256"], image_obj["rootfs_sha256"]]
        if any(not isinstance(value, str) or not SHA256_RE.fullmatch(value) for value in hashes):
            raise BrokerError("invalid_config", "image digests are invalid", 503)
        image = Image(name, image_paths[0], image_paths[1], hashes[0], hashes[1])
        _trusted_artifact(image.kernel, image.kernel_sha256, f"image {name} kernel")
        _trusted_artifact(image.rootfs, image.rootfs_sha256, f"image {name} rootfs")
        images[name] = image
    if os.geteuid() == 0:
        raise BrokerError("invalid_config", "the broker must run as a non-root user", 503)
    return BrokerConfig(paths["socket"], paths["state_dir"], paths["firecracker"], images, memory_mib, max_memory_mib, max_guests, float(timeout))


class _Vsock:
    def __init__(self, endpoint: Path, timeout: float):
        self.endpoint = endpoint
        self.timeout = timeout
        self.sock: socket.socket | None = None
        self._deadline: float | None = None

    def __enter__(self) -> Self:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._deadline = time.monotonic() + self.timeout
        try:
            _set_socket_deadline(sock, self._deadline)
            sock.connect(str(self.endpoint))
            _set_socket_deadline(sock, self._deadline)
            sock.sendall(b"CONNECT 1024\n")
            response = bytearray()
            while not response.endswith(b"\n") and len(response) < 64:
                _set_socket_deadline(sock, self._deadline)
                chunk = sock.recv(64 - len(response))
                if not chunk:
                    raise BrokerError("guest_unavailable", "Firecracker vsock endpoint closed", 503)
                response.extend(chunk)
            if not response.startswith(b"OK"):
                raise BrokerError("guest_unavailable", "Firecracker vsock connection was not accepted", 503)
            self.sock = sock
            return self
        except BaseException:
            sock.close()
            self._deadline = None
            raise

    def __exit__(self, *_: object) -> None:
        if self.sock is not None:
            self.sock.close()
            self.sock = None
        self._deadline = None

    def request(self, value: Mapping[str, Any]) -> dict[str, Any]:
        if self.sock is None:
            raise BrokerError("guest_unavailable", "vsock is not connected", 503)
        payload = json.dumps(value, separators=(",", ":")).encode()
        if len(payload) > MAX_FRAME:
            raise BrokerError("invalid_request", "guest frame is too large")
        deadline = self._deadline
        assert deadline is not None
        _set_socket_deadline(self.sock, deadline)
        self.sock.sendall(len(payload).to_bytes(4, "big") + payload)
        header = _recv_exact(self.sock, 4, deadline)
        length = int.from_bytes(header, "big")
        if length > MAX_FRAME:
            raise BrokerError("guest_protocol", "guest response is too large", 503)
        try:
            envelope = json.loads(_recv_exact(self.sock, length, deadline))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise BrokerError("guest_protocol", "guest response is malformed", 503) from exc
        if not isinstance(envelope, dict):
            raise BrokerError("guest_protocol", "guest response is malformed", 503)
        if set(envelope) == {"error"}:
            error = envelope["error"]
            code = error.get("code") if isinstance(error, dict) else None
            if not isinstance(code, str) or not code:
                code = "guest_error"
            raise BrokerError(code, "guest operation failed", 409)
        result = envelope.get("result")
        if set(envelope) != {"result"} or not isinstance(result, dict):
            raise BrokerError("guest_protocol", "guest response is malformed", 503)
        return result


def _set_socket_deadline(sock: socket.socket, deadline: float) -> None:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("socket deadline expired")
    sock.settimeout(remaining)


def _recv_exact(sock: socket.socket, amount: int, deadline: float | None = None) -> bytes:
    data = bytearray()
    while len(data) < amount:
        if deadline is not None:
            _set_socket_deadline(sock, deadline)
        block = sock.recv(min(65536, amount - len(data)))
        if not block:
            raise BrokerError("guest_unavailable", "guest closed its vsock connection", 503)
        data.extend(block)
    return bytes(data)


def _proc_starttime(pid: int) -> int:
    if sys.platform != "linux":
        raise BrokerError("invalid_config", "Firecracker parent-death containment requires Linux", 503)
    try:
        stat_line = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
        close = stat_line.rfind(")")
        return int(stat_line[close + 2:].split()[19])
    except (OSError, ValueError, IndexError) as exc:
        raise BrokerError("invalid_config", "could not verify broker process identity", 503) from exc
class Broker:
    def __init__(self, config: BrokerConfig):
        self.config = config
        self._lock = threading.RLock()
        self._lifecycle_lock = threading.Lock()
        self._lifecycle_condition = threading.Condition(self._lock)
        self._startup_state = "idle"
        self._guests: dict[str, Guest] = {}
        self._server: _UnixHTTPServer | None = None
        self._stopping = False
        # Linux parent-death signals follow the spawning thread, not its TGID.
        self._spawner = ThreadPoolExecutor(max_workers=1, thread_name_prefix="reach-spawn")
        self._lock_fds: list[int] = []
        self._socket_identity: tuple[int, int] | None = None
        self._serving = False

    def _resolve(self, target: str) -> Guest:
        _validate_target(target)
        with self._lock:
            return self._resolve_locked(target)

    def _resolve_locked(self, target: str) -> Guest:
        if UUID_RE.fullmatch(target):
            guest = self._guests.get(target)
            if guest is None:
                raise BrokerError("not_found", "sandbox was not found", 404)
            if guest.status == "stopping":
                raise BrokerError("unavailable", "sandbox is stopping", 409)
            return guest
        for guest in self._guests.values():
            if guest.name == target:
                if guest.status == "stopping":
                    raise BrokerError("unavailable", "sandbox is stopping", 409)
                return guest
        raise BrokerError("not_found", "sandbox was not found", 404)

    def _sandbox_wire(self, guest: Guest) -> dict[str, Any]:
        return {
            "name": guest.name, "container_id": guest.runtime_id,
            "status": "unknown" if guest.status == "stopping" else guest.status,
            "image": guest.config["image"],
            "ports": {"vnc": None, "novnc": guest.config["ports"]["novnc"], "health": None,
                      "screens": guest.config["screens"], "extra": []},
            "created_at": guest.created_at, "allow_exec": guest.config["allow_exec"],
        }
    def start(self) -> None:
        if os.geteuid() == 0:
            raise BrokerError("invalid_config", "the broker must run as a non-root user", 503)
        if sys.platform != "linux" or not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise BrokerError("invalid_config", "Linux pidfd support is required", 503)
        probe_fd = os.pidfd_open(os.getpid())
        os.close(probe_fd)
        with self._lifecycle_lock:
            with self._lock:
                if self._stopping:
                    raise BrokerError("unavailable", "broker is stopping", 503)
                self._startup_state = "starting"
            lock_fds: list[int] = []
            server: _UnixHTTPServer | None = None
            socket_identity: tuple[int, int] | None = None
            try:
                for directory in (self.config.state_dir, self.config.socket.parent, self.config.state_dir / "guests"):
                    if directory.exists():
                        metadata = directory.lstat()
                        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid():
                            raise BrokerError("invalid_config", "broker directory is not privately owned", 503)
                    else:
                        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
                    os.chmod(directory, 0o700)
                lock_paths = {
                    self.config.state_dir / "broker.lock",
                    self.config.socket.with_name(self.config.socket.name + ".lock"),
                }
                for path in sorted(lock_paths):
                    descriptor = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
                    metadata = os.fstat(descriptor)
                    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
                        os.close(descriptor)
                        raise BrokerError("invalid_config", "broker lock is not privately owned", 503)
                    try:
                        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except OSError as error:
                        os.close(descriptor)
                        raise BrokerError("busy", "broker state or socket is already owned", 409) from error
                    lock_fds.append(descriptor)
                guests_dir = self.config.state_dir / "guests"
                # A prior broker's owned state cannot be safely reconstructed without
                # proving that its Firecracker child is still ours.  Refuse startup
                # without deleting anything; unrelated state remains untouched.
                if any(guests_dir.iterdir()):
                    raise BrokerError("invalid_config", "unreconciled guest state exists", 503)
                if self.config.socket.exists() or self.config.socket.is_symlink():
                    metadata = self.config.socket.lstat()
                    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.geteuid():
                        raise BrokerError("invalid_config", "broker socket path is not privately owned", 503)
                    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                        probe.settimeout(1)
                        try:
                            probe.connect(str(self.config.socket))
                        except ConnectionRefusedError:
                            pass
                        else:
                            raise BrokerError("busy", "broker socket is already serving", 409)
                    self.config.socket.unlink()
                server = _UnixHTTPServer(str(self.config.socket), self)
                metadata = self.config.socket.lstat()
                socket_identity = (metadata.st_dev, metadata.st_ino)
                os.chmod(self.config.socket, 0o600)
                with self._lock:
                    self._server = server
                    self._socket_identity = socket_identity
                    self._lock_fds = lock_fds
                    self._serving = True
                    self._startup_state = "published"
                    self._lifecycle_condition.notify_all()
            except BaseException:
                if server is not None:
                    server.server_close()
                if socket_identity is not None:
                    try:
                        metadata = self.config.socket.lstat()
                        if (metadata.st_dev, metadata.st_ino) == socket_identity:
                            self.config.socket.unlink()
                    except FileNotFoundError:
                        pass
                for descriptor in lock_fds:
                    os.close(descriptor)
                with self._lock:
                    self._server = None
                    self._socket_identity = None
                    self._lock_fds.clear()
                    self._serving = False
                    self._startup_state = "idle"
                    self._lifecycle_condition.notify_all()
                raise
        with self._lock:
            if self._stopping:
                self._serving = False
                self._startup_state = "aborted"
                self._lifecycle_condition.notify_all()
                raise BrokerError("unavailable", "broker is stopping", 503)
            self._startup_state = "serving"
            self._lifecycle_condition.notify_all()
        try:
            assert server is not None
            server.serve_forever(poll_interval=0.2)
        finally:
            with self._lock:
                self._serving = False
                self._startup_state = "idle"
                self._lifecycle_condition.notify_all()

    def stop(self) -> None:
        with self._lifecycle_lock:
            with self._lock:
                already_stopping = self._stopping
                if already_stopping:
                    guests = list(self._guests.values())
                    server = None
                else:
                    self._stopping = True
                    server = self._server
                    guests = list(self._guests.values())
                    for guest in guests:
                        guest.status = "stopping"
                    if self._startup_state == "published":
                        self._startup_state = "stop_requested"
                        self._lifecycle_condition.notify_all()
            if not already_stopping:
                with self._lock:
                    while self._startup_state == "stop_requested":
                        self._lifecycle_condition.wait()
        if server is not None and self._serving:
            server.shutdown()
        failures = []
        try:
            for guest in guests:
                try:
                    self._destroy_guest(guest)
                    if guest.monitor is not None and guest.monitor.is_alive():
                        guest.monitor.join()
                    with self._lock:
                        if guest.cleaned and self._guests.get(guest.runtime_id) is guest:
                            self._guests.pop(guest.runtime_id, None)
                except (OSError, subprocess.TimeoutExpired) as error:
                    failures.append(error)
        finally:
            if not already_stopping:
                self._spawner.shutdown(wait=True)
                if server is not None:
                    server.server_close()
                if self._socket_identity is not None:
                    try:
                        metadata = self.config.socket.lstat()
                        if (metadata.st_dev, metadata.st_ino) == self._socket_identity:
                            self.config.socket.unlink()
                    except FileNotFoundError:
                        pass
                for descriptor in self._lock_fds:
                    os.close(descriptor)
                self._lock_fds.clear()
                with self._lock:
                    self._server = None
                    self._socket_identity = None
                    self._serving = False
                    self._startup_state = "idle"
                    self._lifecycle_condition.notify_all()
        if failures:
            raise RuntimeError("broker resource cleanup failed", failures) from failures[0]

    def dispatch(self, method: str, params: dict[str, Any]) -> Any:
        if method == "list":
            with self._lock:
                if self._stopping:
                    raise BrokerError("unavailable", "broker is stopping", 503)
                return [self._sandbox_wire(g) for g in self._guests.values()]
        if method == "create":
            return self.create(params["config"])
        if method == "destroy":
            self.destroy(params["target"])
            return None
        with self._lock:
            if self._stopping:
                raise BrokerError("unavailable", "broker is stopping", 503)
            guest = self._resolve_locked(params["target"])
            if method == "inspect_config":
                return guest.config
            if method == "incarnation":
                return guest.runtime_id
            if method == "exec_input":
                return self.exec_input(guest, params["command"], params["input_base64"])
        raise BrokerError("invalid_request", "unknown RPC method")

    def create(self, raw_config: Any) -> dict[str, Any]:
        config = _parse_sandbox_config(raw_config, self.config.images, self.config.max_memory_mib)
        with self._lock:
            if self._stopping:
                raise BrokerError("unavailable", "broker is stopping", 503)
            if len(self._guests) >= self.config.max_guests:
                raise BrokerError("busy", "maximum guest count reached", 409)
            if any(g.name == config["name"] for g in self._guests.values()):
                raise BrokerError("conflict", "sandbox name is already in use", 409)
            image = self.config.images[config["image"]]
            _trusted_artifact(image.kernel, image.kernel_sha256, f"image {image.name} kernel")
            _trusted_artifact(image.rootfs, image.rootfs_sha256, f"image {image.name} rootfs")
            runtime_id = str(uuid.uuid4())
            work_dir = self.config.state_dir / "guests" / runtime_id
            rootfs = work_dir / "rootfs.ext4"
            vsock_path = work_dir / "vsock.sock"
            guest_cid = 3 + (uuid.UUID(runtime_id).int % 100000)
            process: subprocess.Popen[bytes] | None = None
            pidfd: int | None = None
            log_file: Any = None
            guest: Guest | None = None
            try:
                work_dir.mkdir(mode=0o700, parents=True)
                (work_dir / ".reach-owned").write_text(runtime_id, encoding="ascii")
                os.chmod(work_dir / ".reach-owned", 0o600)
                shutil.copyfile(image.rootfs, rootfs)
                os.chmod(rootfs, 0o600)
                firecracker_config = self._firecracker_config(config, image, rootfs, vsock_path, guest_cid)
                config_path = work_dir / "firecracker.json"
                config_path.write_text(json.dumps(firecracker_config, separators=(",", ":")), encoding="utf-8")
                os.chmod(config_path, 0o600)
                log_file = (work_dir / "firecracker.log").open("ab", buffering=0)
                parent_pid = os.getpid()
                parent_starttime = _proc_starttime(parent_pid)
                bootstrap = [
                    sys.executable, str(Path(__file__).resolve()), "--firecracker-bootstrap",
                    str(parent_pid), str(parent_starttime), str(self.config.firecracker),
                    "--config-file", str(config_path), "--api-sock", str(work_dir / "api.sock"),
                ]
                process, pidfd = self._spawner.submit(_spawn_owned, bootstrap, work_dir, log_file).result()
                guest = Guest(config["name"], config, runtime_id, image, work_dir, rootfs, vsock_path, process,
                              pidfd, log_file, datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"), guest_cid)
                self._guests[runtime_id] = guest
                self._wait_ready(guest)
                if guest.process.poll() is not None:
                    raise BrokerError("guest_failed", "Firecracker exited before publication", 503)
                self._start_forwarders(guest)
                guest.status = "running"
                guest.monitor = threading.Thread(
                    target=self._monitor, args=(guest,), daemon=False,
                    name=f"reach-monitor-{runtime_id[:8]}",
                )
                guest.monitor.start()
                return self._sandbox_wire(guest)
            except Exception:
                if guest is not None:
                    # Keep the exact process/pidfd registered while failed
                    # cleanup remains retryable by stop or reconciliation.
                    guest.status = "stopping"
                    self._destroy_guest(guest)
                    if guest.cleaned and self._guests.get(runtime_id) is guest:
                        self._guests.pop(runtime_id, None)
                else:
                    if process is not None and pidfd is not None:
                        _terminate_owned(process, pidfd)
                        os.close(pidfd)
                    if log_file is not None:
                        log_file.close()
                    if work_dir.exists():
                        shutil.rmtree(work_dir)
                raise

    def _firecracker_config(self, config: dict[str, Any], image: Image, rootfs: Path, vsock: Path, guest_cid: int) -> dict[str, Any]:
        memory_mib = config["memory"] // (1024 * 1024) if config["memory"] is not None else self.config.memory_mib
        return {
            "boot-source": {"kernel_image_path": str(image.kernel), "boot_args": "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/init"},
            "drives": [{"drive_id": "rootfs", "path_on_host": str(rootfs), "is_root_device": True, "is_read_only": False}],
            "machine-config": {"vcpu_count": 1, "mem_size_mib": memory_mib, "smt": False},
            "vsock": {"guest_cid": guest_cid, "uds_path": str(vsock)},
        }

    def _guest_call(self, guest: Guest, value: Mapping[str, Any], timeout: float = 10) -> dict[str, Any]:
        try:
            with _Vsock(guest.vsock_path, timeout) as channel:
                return channel.request(value)
        except BrokerError:
            raise
        except (OSError, TimeoutError, ValueError) as exc:
            raise BrokerError("guest_unavailable", "guest transport is unavailable", 503) from exc

    def _wait_ready(self, guest: Guest) -> None:
        deadline = time.monotonic() + min(60.0, self.config.exec_timeout_seconds)
        while time.monotonic() < deadline:
            if guest.process.poll() is not None:
                raise BrokerError("guest_failed", "Firecracker exited before guest readiness", 503)
            try:
                self._guest_call(guest, {"op": "health"}, timeout=min(1, max(0.1, deadline - time.monotonic())))
                break
            except (BrokerError, OSError):
                time.sleep(min(0.1, max(0, deadline - time.monotonic())))
        else:
            raise BrokerError("guest_unavailable", "guest did not become ready", 503)
        # Configure is a one-shot mutation.  Once sent, never replay it after
        # an uncertain transport result.  Supervisor health is read-only and
        # may be polled until the same outer readiness deadline.
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise BrokerError("guest_unavailable", "guest did not become ready", 503)
        self._guest_call(
            guest,
            {"op": "configure", "width": guest.config["resolution"]["width"], "height": guest.config["resolution"]["height"], "screens": guest.config["screens"], "vnc_password": guest.config["vnc_password"]},
            timeout=min(10, remaining),
        )
        while time.monotonic() < deadline:
            if guest.process.poll() is not None:
                raise BrokerError("guest_failed", "Firecracker exited during supervisor readiness", 503)
            remaining = deadline - time.monotonic()
            if remaining < 0.1:
                break
            try:
                result = self._guest_call(
                    guest,
                    {"op": "exec", "command": ["curl", "-fsS", "--max-time", "2", "http://127.0.0.1:8400/health"], "stdin_base64": "", "timeout_seconds": min(2, remaining)},
                    timeout=min(2, remaining),
                )
                if result.get("exit_code") == 0:
                    try:
                        health = json.loads(result.get("stdout", ""))
                    except (ValueError, TypeError):
                        health = None
                    if isinstance(health, dict) and health.get("status") == "healthy":
                        return
            except BrokerError:
                pass
            time.sleep(min(0.1, max(0, deadline - time.monotonic())))
        raise BrokerError("guest_unhealthy", "guest supervisor health check failed", 503)

    def _monitor(self, guest: Guest) -> None:
        returncode = guest.process.wait()
        with self._lock:
            current = self._guests.get(guest.runtime_id)
            if current is not guest:
                return
            if guest.status == "stopping":
                self._cleanup_guest(guest)
                self._guests.pop(guest.runtime_id, None)
                return
            guest.status = "stopped" if returncode == 0 else "unhealthy"
            if self._stopping:
                return
            self._cleanup_guest(guest)
            self._guests.pop(guest.runtime_id, None)

    def exec_input(self, guest: Guest, command: Any, input_base64: Any) -> dict[str, Any]:
        _validate_command(command)
        input_bytes = _decode_input(input_base64)
        if guest.process.poll() is not None:
            raise BrokerError("unavailable", "guest process has exited", 503)
        transport_timeout = min(
            MAX_EXEC_TRANSPORT_SECONDS,
            self.config.exec_timeout_seconds + EXEC_TRANSPORT_MARGIN_SECONDS,
        )
        return self._guest_call(
            guest,
            {"op": "exec", "command": command, "stdin_base64": base64.b64encode(input_bytes).decode("ascii"),
             "timeout_seconds": self.config.exec_timeout_seconds},
            transport_timeout,
        )

    def destroy(self, target: str) -> None:
        with self._lock:
            if self._stopping:
                raise BrokerError("unavailable", "broker is stopping", 503)
            guest = self._resolve_locked(target)
            guest.status = "stopping"
        try:
            self._destroy_guest(guest)
        except subprocess.TimeoutExpired as exc:
            # Keep the exact pidfd and stopping registry entry for the monitor.
            # A later process exit lets it reclaim the owned resources without
            # exposing a PID-reuse window or requiring a mutation retry.
            raise BrokerError("unavailable", "guest termination did not complete", 503) from exc
        if guest.monitor is not None and guest.monitor.is_alive():
            guest.monitor.join()
        with self._lock:
            if self._guests.get(guest.runtime_id) is guest:
                self._guests.pop(guest.runtime_id, None)

    def _destroy_guest(self, guest: Guest) -> None:
        with self._lock:
            if not guest.cleaned:
                _terminate_owned(guest.process, guest.pidfd)
                self._cleanup_guest(guest)

    def _cleanup_guest(self, guest: Guest) -> None:
        with self._lock:
            if guest.cleaned:
                return
            for listener, thread in guest.forwarders:
                listener.close()
                thread.join(timeout=2)
            guest.forwarders.clear()
            guest.log_file.close()
            if guest.work_dir.exists():
                shutil.rmtree(guest.work_dir)
                if guest.work_dir.exists():
                    raise OSError("guest work directory remains after cleanup")
            # Keep the pidfd open until the owned work tree is gone so a
            # cleanup retry can never signal a reused numeric PID.
            os.close(guest.pidfd)
            guest.cleaned = True

    def _start_forwarders(self, guest: Guest) -> None:
        # Only noVNC guest loopback ports are forwarded.  Raw VNC, CDP, health,
        # and arbitrary extra ports never receive a host listener.
        for screen in range(guest.config["screens"]):
            host_port = guest.config["ports"]["novnc"] + screen
            try:
                listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind(("127.0.0.1", host_port))
                listener.listen(8)
            except OSError as exc:
                if "listener" in locals():
                    listener.close()
                raise BrokerError("port_unavailable", "noVNC loopback port is unavailable", 409) from exc
            thread = threading.Thread(target=self._forward_loop, args=(guest, listener, 6080 + screen), daemon=True, name=f"reach-novnc-{guest.runtime_id[:8]}-{screen}")
            guest.forwarders.append((listener, thread))
            thread.start()

    def _forward_loop(self, guest: Guest, listener: socket.socket, port: int) -> None:
        listener.settimeout(0.5)
        while not self._stopping and guest.process.poll() is None:
            try:
                client, _ = listener.accept()
            except TimeoutError:
                continue
            except OSError:
                return
            threading.Thread(target=self._forward_one, args=(guest, client, port), daemon=True).start()

    def _forward_one(self, guest: Guest, client: socket.socket, port: int) -> None:
        try:
            # Only the guest-loopback noVNC range can be forwarded.
            with client, _Vsock(guest.vsock_path, 10) as channel:
                result = channel.request({"op": "connect", "port": port})
                if result.get("connected") is not True:
                    return
                _bridge(client, channel.sock)
        except (OSError, BrokerError):
            return


def _bridge(left: socket.socket, right: socket.socket | None) -> None:
    if right is None:
        return
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
            destination = right if source is left else left
            destination.sendall(data)


def _spawn_owned(command: list[str], directory: Path, log_file: Any) -> tuple[subprocess.Popen[bytes], int]:
    process = subprocess.Popen(
        command, cwd=directory, stdin=subprocess.DEVNULL, stdout=log_file,
        stderr=subprocess.STDOUT, start_new_session=True, close_fds=True,
    )
    try:
        return process, os.pidfd_open(process.pid)
    except OSError:
        # Nothing else can observe or reap this unpublished child.
        process.kill()
        process.wait()
        raise


def _terminate_owned(process: subprocess.Popen[bytes], pidfd: int) -> None:
    try:
        signal.pidfd_send_signal(pidfd, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
        return
    except subprocess.TimeoutExpired:
        pass
    try:
        signal.pidfd_send_signal(pidfd, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=2)


class _UnixHTTPServer(http.server.ThreadingHTTPServer):
    address_family = socket.AF_UNIX
    daemon_threads = False
    allow_reuse_address = False

    def __init__(self, path: str, broker: Broker):
        self.broker = broker
        super().__init__(path, _RPCHandler)


class _RPCHandler(http.server.BaseHTTPRequestHandler):
    server: _UnixHTTPServer
    protocol_version = "HTTP/1.1"

    def setup(self) -> None:
        self.request.settimeout(10)
        super().setup()

    def log_message(self, *_: Any) -> None:
        return

    def do_POST(self) -> None:
        if self.path != "/v1/rpc":
            self._error(BrokerError("not_found", "route was not found", 404))
            return
        if not _peer_uid_allowed(self.connection):
            self._error(BrokerError("forbidden", "broker socket peer is not authorized", 403))
            return
        try:
            length = int(self.headers.get("Content-Length", "-1"))
            if length < 0 or length > MAX_FRAME:
                raise BrokerError("invalid_request", "request body is too large")
            raw = json.loads(self.rfile.read(length))
            method, params = validate_rpc_request(raw, self.server.broker.config.images, self.server.broker.config.max_memory_mib)
        except BrokerError as exc:
            self._error(exc)
            return
        except (OSError, ValueError):
            self._error(BrokerError("invalid_request", "request body is malformed"))
            return
        try:
            result = self.server.broker.dispatch(method, params)
            self._json(200, {"result": result})
        except BrokerError as exc:
            self._error(exc)
        except Exception:
            LOGGER.exception("broker request failed")
            self._error(BrokerError("unavailable", "broker request failed", 503))

    def _json(self, status: int, value: Mapping[str, Any]) -> None:
        payload = json.dumps(value, separators=(",", ":")).encode()
        if len(payload) > MAX_FRAME:
            self._error(BrokerError("unavailable", "response is too large", 503))
            return
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def _error(self, error: BrokerError) -> None:
        self._json(error.status, {"error": {"code": error.code, "message": error.message}})


def _peer_uid_allowed(connection: socket.socket) -> bool:
    if not hasattr(socket, "SO_PEERCRED"):
        return False
    try:
        credentials = connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
        uid = int.from_bytes(credentials[4:8], "little")
    except OSError:
        return False
    return uid == os.geteuid() and uid != 0





def _firecracker_bootstrap(arguments: list[str]) -> int:
    if len(arguments) < 3:
        return os.EX_USAGE
    try:
        expected_parent = int(arguments[0])
        expected_starttime = int(arguments[1])
    except ValueError:
        return os.EX_USAGE
    executable = arguments[2]
    if sys.platform != "linux" or os.getppid() != expected_parent:
        return os.EX_NOPERM
    try:
        if _proc_starttime(expected_parent) != expected_starttime:
            return os.EX_NOPERM
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.prctl(1, signal.SIGKILL, 0, 0, 0) != 0:
            return os.EX_OSERR
        if os.getppid() != expected_parent:
            return os.EX_NOPERM
        os.execv(executable, [executable, *arguments[3:]])
    except (OSError, ValueError, BrokerError):
        return os.EX_OSERR
    return os.EX_OSERR


def main() -> int:
    if len(sys.argv) > 1 and sys.argv[1] == "--firecracker-bootstrap":
        return _firecracker_bootstrap(sys.argv[2:])
    parser = argparse.ArgumentParser(description="Run the private Reach Firecracker broker")
    parser.add_argument("--config", required=True, help="private broker JSON configuration")
    args = parser.parse_args()
    broker = Broker(load_config(args.config))
    stop_thread: threading.Thread | None = None
    stop_errors: list[Exception] = []

    def stop_broker() -> None:
        try:
            broker.stop()
        except Exception as error:
            LOGGER.exception("broker shutdown failed")
            stop_errors.append(error)

    def request_stop(*_signal_args: Any) -> None:
        nonlocal stop_thread
        if stop_thread is None:
            stop_thread = threading.Thread(target=stop_broker, daemon=False, name="reach-broker-stop")
            stop_thread.start()

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    try:
        broker.start()
    finally:
        if stop_thread is not None:
            stop_thread.join()
        else:
            broker.stop()
    if stop_errors:
        raise stop_errors[0]
    return 0



if __name__ == "__main__":
    raise SystemExit(main())
