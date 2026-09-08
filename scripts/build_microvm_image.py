#!/usr/bin/env python3
"""Build a networkless ARM64 Reach guest image inside an owned Linux build VM."""

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

PACKAGES = [
    "systemd-sysv", "dbus", "ca-certificates", "curl", "iproute2", "procps",
    "python3", "python3-venv", "python3-pip", "xvfb", "x11vnc", "openbox",
    "xdotool", "scrot", "xclip", "x11-xserver-utils", "fonts-noto-core",
    "fonts-noto-mono", "dbus-x11", "util-linux", "jq",
]
PYTHON_PACKAGES = [
    "playwright==1.62.0", "websockify==0.13.0", "Pillow==12.3.0",
    "scrapling[fetchers]==0.4.15",
]
BUILD_ENV = {
    "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME": "/root", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
    "DEBIAN_FRONTEND": "noninteractive", "PLAYWRIGHT_BROWSERS_PATH": "/opt/ms-playwright",
}


def validate_output(output, owned_root):
    root = Path(owned_root).resolve(strict=True)
    metadata = root.stat()
    if not root.is_dir() or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
        raise PermissionError("--output-root must be an existing caller-owned private directory")
    output = Path(output)
    if not output.is_absolute():
        output = root / output
    if output.exists() or output.is_symlink():
        raise FileExistsError(f"refusing to replace existing output: {output}")
    output = output.resolve()
    if not output.is_relative_to(root):
        raise ValueError("output is outside the private --output-root")
    return output


def run(command):
    subprocess.run([str(value) for value in command], check=True, env=BUILD_ENV,
                   stdout=sys.stderr, stderr=sys.stderr)


class _StagingDirectory:
    def __init__(self, parent):
        self.path = Path(tempfile.mkdtemp(prefix=".reach-image-", dir=parent))
        self.live_mounts = []

    def __enter__(self):
        return self.path

    def __exit__(self, *_):
        if not self.live_mounts:
            shutil.rmtree(self.path)
        return False


def _unmount_all(mounted):
    failures = []
    remaining = []
    for destination in reversed(mounted):
        try:
            run(["umount", destination])
        except (OSError, subprocess.SubprocessError) as exc:
            failures.append(exc)
            remaining.append(destination)
    mounted[:] = remaining
    return failures


def put(root, relative, content, mode=0o644):
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
    path.chmod(mode)


def build(args):
    output = validate_output(args.output, args.output_root)
    if sys.platform != "linux" or platform.machine() not in ("aarch64", "arm64"):
        raise ValueError("build inside an ARM64 Linux VM, not on the macOS host")
    if os.geteuid() != 0:
        raise PermissionError("image construction requires root inside the owned build VM")
    repo = Path(args.repo).resolve(strict=True)
    supervisor = Path(args.supervisor).resolve(strict=True)
    with supervisor.open("rb") as stream:
        header = stream.read(20)
    if header[:4] != b"\x7fELF" or int.from_bytes(header[18:20], "little") != 183:
        raise ValueError("--supervisor must be a Linux ARM64 ELF executable")
    for binary in ("debootstrap", "chroot", "mount", "umount", "mkfs.ext4"):
        if shutil.which(binary, path=BUILD_ENV["PATH"]) is None:
            raise ValueError(f"required image-build command is missing: {binary}")
    sources = {
        "scripts/reach-chrome": "usr/local/bin/reach-chrome",
        "scripts/reach-wallpaper": "usr/local/bin/reach-wallpaper",
        "scripts/reach-home": "usr/local/bin/reach-home",
        "scripts/reach_viewer_auth.py": "opt/reach/reach_viewer_auth.py",
        "scripts/reach_microvm_guest.py": "usr/local/lib/reach/reach_microvm_guest.py",
        "assets/home.html": "opt/reach/home.html",
        "config/openbox-rc.xml": "home/sandbox/.config/openbox/rc.xml",
        "config/chrome-policies.json": "etc/chromium/policies/managed/reach.json",
    }
    for source in sources:
        if not (repo / source).is_file():
            raise ValueError(f"required guest source is missing: {source}")
    output.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    staging = _StagingDirectory(output.parent)
    with staging as scratch:
        scratch = Path(scratch)
        root = scratch / "root"
        run(["debootstrap", "--arch=arm64", "--variant=minbase", args.suite, root,
             "https://deb.debian.org/debian"])
        put(root, "usr/sbin/policy-rc.d", "#!/bin/sh\nexit 101\n", 0o755)
        shutil.copyfile("/etc/resolv.conf", root / "etc/resolv.conf")
        mounted = []
        try:
            for filesystem, source, target in (("proc", "proc", "proc"),
                                                ("sysfs", "sysfs", "sys"),
                                                ("devpts", "devpts", "dev/pts")):
                destination = root / target
                destination.mkdir(parents=True, exist_ok=True)
                run(["mount", "-t", filesystem, source, destination])
                mounted.append(destination)
            run(["chroot", root, "apt-get", "update"])
            run(["chroot", root, "apt-get", "install", "-y", "--no-install-recommends", *PACKAGES])
            run(["chroot", root, "python3", "-m", "venv", "/opt/reach-venv"])
            run(["chroot", root, "/opt/reach-venv/bin/pip", "install", *PYTHON_PACKAGES])
            run(["chroot", root, "/opt/reach-venv/bin/playwright", "install-deps", "chromium"])
            run(["chroot", root, "/opt/reach-venv/bin/playwright", "install", "chromium"])
            run(["chroot", root, "useradd", "--create-home", "--shell", "/bin/bash", "sandbox"])
            for source, destination in sources.items():
                target = root / destination
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(repo / source, target)
                target.chmod(0o755 if destination.startswith("usr/local/bin/") else 0o644)
            target = root / "usr/local/bin/reach-supervisor"
            shutil.copyfile(supervisor, target)
            target.chmod(0o755)
            compatibility = subprocess.run(["chroot", str(root), "ldd", "/usr/local/bin/reach-supervisor"],
                                           capture_output=True, text=True, env=BUILD_ENV, check=False)
            if "not found" in compatibility.stdout + compatibility.stderr:
                raise RuntimeError("supervisor ABI is incompatible with the guest; rebuild against its libc")
            run(["chroot", root, "install", "-d", "-o", "sandbox", "-g", "sandbox", "-m", "0700", "/run/reach"])
            run(["chroot", root, "install", "-d", "-m", "1777", "/tmp/.X11-unix"])
            run(["chroot", root, "chown", "-R", "sandbox:sandbox", "/home/sandbox"])
            run(["chroot", root, "chmod", "-R", "a+rX", "/opt/ms-playwright", "/opt/reach-venv"])
            run(["chroot", root, "apt-get", "clean"])
            packages = subprocess.check_output(["chroot", str(root), "dpkg-query", "-W",
                                                "-f=${Package}=${Version}\n"], text=True, env=BUILD_ENV)
            python_packages = subprocess.check_output(["chroot", str(root), "/opt/reach-venv/bin/pip", "freeze"],
                                                       text=True, env=BUILD_ENV)
        finally:
            failures = _unmount_all(mounted)
            staging.live_mounts[:] = mounted
            if failures and sys.exc_info()[0] is None:
                raise RuntimeError("image build mount cleanup failed") from failures[0]
        put(root, "etc/machine-id", "")
        put(root, "etc/hostname", "reach-microvm\n")
        put(root, "etc/hosts", "127.0.0.1 localhost\n::1 localhost\n")
        put(root, "etc/resolv.conf", "")
        put(root, "etc/fstab", "tmpfs /dev/shm tmpfs nosuid,nodev,noexec,mode=1777,size=512M 0 0\n")
        put(root, "etc/systemd/system/reach-microvm-guest.service", """[Unit]
Description=Reach private vsock guest transport
After=local-fs.target
[Service]
Type=simple
User=sandbox
Group=sandbox
RuntimeDirectory=reach
RuntimeDirectoryMode=0700
NoNewPrivileges=yes
ExecStartPre=+/usr/sbin/ip link set lo up
ExecStartPre=+/usr/bin/install -d -m 1777 /tmp/.X11-unix
ExecStart=/usr/bin/python3 /usr/local/lib/reach/reach_microvm_guest.py
Restart=no
Environment=PATH=/opt/reach-venv/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
Environment=PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright
[Install]
WantedBy=multi-user.target
""")
        wants = root / "etc/systemd/system/multi-user.target.wants"
        wants.mkdir(parents=True, exist_ok=True)
        (wants / "reach-microvm-guest.service").symlink_to("../reach-microvm-guest.service")
        (root / "usr/sbin/policy-rc.d").unlink()
        seed = root / "var/lib/systemd/random-seed"
        seed.unlink(missing_ok=True)
        for path in (root / "var/lib/apt/lists").iterdir():
            if path.is_dir():
                shutil.rmtree(path)
            else:
                path.unlink()
        staged = scratch / "rootfs.ext4"
        with staged.open("xb") as stream:
            stream.truncate(args.size_mib * 1024 * 1024)
        run(["mkfs.ext4", "-F", "-q", "-d", root, staged])
        with staged.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        os.link(staged, output)
        output.chmod(0o600)
    return {"rootfs": str(output), "sha256": digest, "size_mib": args.size_mib,
            "suite": args.suite, "packages": packages.splitlines(),
            "python_packages": python_packages.splitlines(), "network": "no NIC"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--supervisor", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--output-root", required=True)
    parser.add_argument("--suite", choices=("bookworm", "trixie"), default="bookworm")
    parser.add_argument("--size-mib", type=int, default=4096)
    args = parser.parse_args()
    if not 2048 <= args.size_mib <= 16384:
        parser.error("--size-mib must be between 2048 and 16384")
    try:
        print(json.dumps(build(args)))
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(json.dumps({"error": str(error)}))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
