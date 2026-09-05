"""Obscura Browser Hermes Plugin.

Connects Hermes agents to Obscura, the 51ms Rust-native headless browser
with built-in anti-fingerprinting stealth, 3,520 tracker blocks, and V8 JS execution.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from typing import Any, Dict, List, Optional


def _resolve_obscura_bin() -> Optional[str]:
    """Locate obscura binary in PATH or common build locations."""
    in_path = shutil.which("obscura")
    if in_path:
        return in_path

    # Check local repo builds or standard release paths
    candidates = [
        os.path.expanduser("~/repos/obscura/target/release/obscura"),
        os.path.expanduser("~/repos/obscura/target/debug/obscura"),
        "/usr/local/bin/obscura",
        "/usr/bin/obscura",
    ]
    for c in candidates:
        if os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    return None


def obscura_fetch(
    url: str,
    dump: str = "markdown",
    stealth: bool = True,
    wait_until: str = "load",
    timeout: int = 30,
) -> Dict[str, Any]:
    """Fetch and render a URL using Obscura's fast Rust V8 engine."""
    bin_path = _resolve_obscura_bin()
    if not bin_path:
        return {
            "error": "obscura_not_found",
            "message": "obscura binary not found in PATH or standard build paths.",
        }

    cmd = [
        bin_path,
        "fetch",
        url,
        "--dump",
        dump,
        "--wait-until",
        wait_until,
        "--timeout",
        str(timeout),
        "--quiet",
    ]
    if stealth:
        cmd.append("--stealth")

    try:
        proc = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout + 5,
        )
        if proc.returncode != 0:
            return {
                "error": "fetch_failed",
                "exit_code": proc.returncode,
                "stderr": proc.stderr.strip(),
            }
        return {
            "status": "success",
            "url": url,
            "format": dump,
            "content": proc.stdout,
        }
    except subprocess.TimeoutExpired:
        return {
            "error": "timeout",
            "message": f"Navigation exceeded {timeout}s deadline.",
        }
    except Exception as exc:
        return {"error": "execution_error", "message": str(exc)}


def obscura_eval(
    url: str,
    expr: str,
    stealth: bool = True,
) -> Dict[str, Any]:
    """Evaluate a JavaScript expression on target URL using Obscura V8."""
    bin_path = _resolve_obscura_bin()
    if not bin_path:
        return {
            "error": "obscura_not_found",
            "message": "obscura binary not found in PATH.",
        }

    cmd = [bin_path, "fetch", url, "--eval", expr, "--quiet"]
    if stealth:
        cmd.append("--stealth")

    try:
        proc = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=35,
        )
        if proc.returncode != 0:
            return {
                "error": "eval_failed",
                "exit_code": proc.returncode,
                "stderr": proc.stderr.strip(),
            }
        return {
            "status": "success",
            "url": url,
            "expression": expr,
            "result": proc.stdout.strip(),
        }
    except Exception as exc:
        return {"error": "execution_error", "message": str(exc)}


def obscura_scrape(
    urls: List[str],
    expr: Optional[str] = None,
    concurrency: int = 10,
) -> Dict[str, Any]:
    """Scrape multiple URLs in parallel using Obscura worker processes."""
    bin_path = _resolve_obscura_bin()
    if not bin_path:
        return {
            "error": "obscura_not_found",
            "message": "obscura binary not found in PATH.",
        }

    cmd = [
        bin_path,
        "scrape",
        *urls,
        "--concurrency",
        str(concurrency),
        "--format",
        "json",
        "--quiet",
    ]
    if expr:
        cmd.extend(["--eval", expr])

    try:
        proc = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=120,
        )
        if proc.returncode != 0:
            return {
                "error": "scrape_failed",
                "exit_code": proc.returncode,
                "stderr": proc.stderr.strip(),
            }
        try:
            parsed = json.loads(proc.stdout)
            return {"status": "success", "results": parsed}
        except json.JSONDecodeError:
            return {"status": "success", "raw_output": proc.stdout.strip()}
    except Exception as exc:
        return {"error": "execution_error", "message": str(exc)}


def obscura_cdp_info(port: int = 9222) -> Dict[str, Any]:
    """Return Obscura CDP server connection details."""
    host = os.environ.get("OBSCURA_HOST", "127.0.0.1")
    return {
        "host": host,
        "port": port,
        "ws_endpoint": f"ws://{host}:{port}/devtools/browser",
        "http_endpoint": f"http://{host}:{port}",
        "protocol": "Chrome DevTools Protocol (CDP)",
        "features": [
            "51ms page load",
            "anti-detection TLS ClientHello",
            "3520 tracker blocklist",
            "retained CSS layout & screenshot engine",
        ],
    }
