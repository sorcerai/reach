"""Unit tests for Perceptual Hash (pHash / dHash) change gate and ROI cropping."""

import json
import os
from pathlib import Path
import sys
from unittest.mock import MagicMock, patch
import pytest

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_drive import (
    ESTIMATED_COST_PER_VLM_CALL_USD,
    ESTIMATED_TOKENS_PER_VLM_CALL,
    GateDecision,
    PerceptualChangeGate,
    ReachAction,
    ReachDriver,
    Roi,
    StepRecord,
    calculate_visual_change,
    compute_dhash,
    compute_phash,
    crop_image,
    downsample_to_grayscale,
    generate_html_report,
    is_wait_or_scroll_action,
)
import struct
import zlib


def make_test_png(width: int, height: int, rgb: bytes) -> bytes:
    raw = bytearray()
    for _ in range(height):
        raw.append(0)  # filter None
        raw.extend(rgb * width)
    compressed = zlib.compress(raw)

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", compressed)
        + chunk(b"IEND", b"")
    )


def make_gradient_png(width: int, height: int) -> bytes:
    raw = bytearray()
    for y in range(height):
        raw.append(0)
        for x in range(width):
            raw.extend(bytes([x % 256, y % 256, (x + y) % 256]))
    compressed = zlib.compress(raw)

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", compressed)
        + chunk(b"IEND", b"")
    )


# ------------------------------------------------------------------------------
# Image processing & Perceptual Hashing Tests
# ------------------------------------------------------------------------------


def test_downsample_grayscale_dimensions():
    png = make_test_png(40, 30, b"\x64\x64\x64")
    g8 = downsample_to_grayscale(png, target_w=8, target_h=8)
    assert len(g8) == 64
    assert all(val == 100 for val in g8)

    g16 = downsample_to_grayscale(png, target_w=16, target_h=16)
    assert len(g16) == 256
    assert all(val == 100 for val in g16)


def test_identical_images_have_zero_visual_change():
    png1 = make_test_png(50, 50, b"\x32\x32\x32")
    png2 = make_test_png(50, 50, b"\x32\x32\x32")
    diff = calculate_visual_change(png1, png2, size=16)
    assert diff == 0.0

    dh1 = compute_dhash(png1, size=8)
    dh2 = compute_dhash(png2, size=8)
    assert dh1 == dh2

    ph1 = compute_phash(png1, size=8)
    ph2 = compute_phash(png2, size=8)
    assert ph1 == ph2


def test_distinct_images_have_positive_visual_change():
    black = make_test_png(50, 50, b"\x00\x00\x00")
    white = make_test_png(50, 50, b"\xff\xff\xff")
    diff = calculate_visual_change(black, white, size=16)
    assert abs(diff - 1.0) < 1e-4

    grad = make_gradient_png(64, 64)
    diff_grad = calculate_visual_change(black, grad, size=16)
    assert 0.05 < diff_grad < 0.95


def test_roi_cropping_and_bounds_clamping(tmp_path: Path):
    grad = make_gradient_png(100, 100)
    out_file = tmp_path / "crop.png"

    # Crop 30x30 region
    roi = Roi(x=10, y=15, width=30, height=30)
    cropped_bytes = crop_image(grad, roi, out_path=out_file)
    assert os.path.isfile(out_file)
    assert cropped_bytes.startswith(b"\x89PNG\r\n\x1a\n")

    # Verify cropped image dimensions from IHDR
    w, h = struct.unpack(">II", cropped_bytes[16:24])
    assert w == 30 and h == 30

    # Crop overflowing bounds (clamps within image)
    over_roi = Roi(x=90, y=85, width=50, height=50)
    over_crop = crop_image(grad, over_roi)
    ow, oh = struct.unpack(">II", over_crop[16:24])
    assert ow == 10 and oh == 15


def test_is_wait_or_scroll_action_detection():
    assert is_wait_or_scroll_action(ReachAction(kind="wait", description="Wait for load"))
    assert is_wait_or_scroll_action(ReachAction(kind="scroll", description="Scroll page"))
    assert is_wait_or_scroll_action(ReachAction(kind="key", key="Page_Down"))
    assert is_wait_or_scroll_action(ReachAction(kind="key", key="Down"))
    assert is_wait_or_scroll_action(ReachAction(kind="click", description="Wait for page animation to settle"))
    assert not is_wait_or_scroll_action(ReachAction(kind="click", description="Submit form"))
    assert not is_wait_or_scroll_action(ReachAction(kind="type", value="hello", description="Type username"))


# ------------------------------------------------------------------------------
# Change Gate State Machine Tests
# ------------------------------------------------------------------------------


def test_change_gate_evaluation_sequence():
    gate = PerceptualChangeGate(
        min_change_threshold=0.01,
        max_unchanged_ticks=3,
        backoff_sec=0.5,
    )

    frame1 = make_test_png(50, 50, b"\x40\x40\x40")
    frame2 = make_test_png(50, 50, b"\x40\x40\x40")  # Identical

    # Tick 1: First observation always invokes VLM
    dec1 = gate.evaluate(frame1, last_action_was_wait_or_scroll=False)
    assert not dec1.should_skip_vlm
    assert dec1.unchanged_ticks == 0

    # Tick 2: Identical frame after wait action -> should skip VLM
    dec2 = gate.evaluate(frame2, last_action_was_wait_or_scroll=True)
    assert dec2.should_skip_vlm
    assert dec2.unchanged_ticks == 1
    assert dec2.backoff_sec == 0.5
    assert dec2.visual_distance == 0.0

    # Tick 3: Identical frame after scroll -> should skip VLM (unchanged_ticks = 2)
    dec3 = gate.evaluate(frame2, last_action_was_wait_or_scroll=True)
    assert dec3.should_skip_vlm
    assert dec3.unchanged_ticks == 2

    # Tick 4: 3rd unchanged tick (max=3 reached) -> should skip VLM
    dec4 = gate.evaluate(frame2, last_action_was_wait_or_scroll=True)
    assert dec4.should_skip_vlm
    assert dec4.unchanged_ticks == 3

    # Tick 5: Exceeds max_unchanged_ticks (3) -> forces VLM invocation!
    dec5 = gate.evaluate(frame2, last_action_was_wait_or_scroll=True)
    assert not dec5.should_skip_vlm
    assert dec5.unchanged_ticks == 0
    assert "Maximum unchanged ticks" in dec5.reason

    assert gate.skipped_vlm_ticks == 3
    assert gate.total_vlm_calls == 2
    assert gate.tokens_saved == 3 * ESTIMATED_TOKENS_PER_VLM_CALL
    assert abs(gate.cost_saved - 3 * ESTIMATED_COST_PER_VLM_CALL_USD) < 1e-6


def test_change_gate_invokes_on_changed_frame():
    gate = PerceptualChangeGate(min_change_threshold=0.01)

    frame1 = make_test_png(50, 50, b"\x10\x10\x10")
    frame2 = make_test_png(50, 50, b"\xf0\xf0\xf0")  # Huge visual change

    dec1 = gate.evaluate(frame1, last_action_was_wait_or_scroll=False)
    assert not dec1.should_skip_vlm

    dec2 = gate.evaluate(frame2, last_action_was_wait_or_scroll=True)
    assert not dec2.should_skip_vlm
    assert dec2.unchanged_ticks == 0
    assert dec2.visual_distance > 0.5


def test_change_gate_invokes_if_action_not_wait():
    gate = PerceptualChangeGate(min_change_threshold=0.01)

    frame1 = make_test_png(50, 50, b"\x50\x50\x50")
    frame2 = make_test_png(50, 50, b"\x50\x50\x50")  # Identical

    gate.evaluate(frame1, last_action_was_wait_or_scroll=False)
    # Frame unchanged, but previous action was click/type (not wait/scroll)
    dec2 = gate.evaluate(frame2, last_action_was_wait_or_scroll=False)
    assert not dec2.should_skip_vlm
    assert dec2.unchanged_ticks == 0


# ------------------------------------------------------------------------------
# Report HTML and Audit Reel Metrics Tests
# ------------------------------------------------------------------------------


def test_report_html_marks_vlm_cached_and_metrics(tmp_path: Path):
    audit_dir = tmp_path / "task_cached_test"
    audit_dir.mkdir(parents=True, exist_ok=True)

    meta = {
        "task_id": "task_audit_phash_001",
        "goal": "Verify pHash change gate and audit reel badge",
        "screen": 0,
        "model": "gemini-3.8-flash-high",
        "status": "completed",
        "success": True,
        "final_description": "Done",
        "duration_sec": 12.5,
        "skipped_vlm_ticks": 3,
        "tokens_saved": 4800,
        "cost_saved": 0.00072,
        "steps": [
            {
                "step_index": 1,
                "action": {"kind": "wait", "description": "Wait for animation"},
                "vlm_cached": True,
                "visual_change": 0.002,
                "observation_summary": "Skipped VLM call due to static screen",
                "timestamp": "2026-09-05T00:00:01Z",
            },
            {
                "step_index": 2,
                "action": {"kind": "terminate", "description": "Goal achieved"},
                "vlm_cached": False,
                "observation_summary": "Terminated",
                "timestamp": "2026-09-05T00:00:02Z",
            },
        ],
    }

    report_path = generate_html_report(audit_dir, meta)
    assert os.path.isfile(report_path)

    with open(report_path, "r", encoding="utf-8") as f:
        content = f.read()

    # Verify badge and metrics in HTML
    assert "VLM CACHED (pHash Gated)" in content
    assert "Skipped VLM Calls" in content
    assert "Tokens Saved" in content
    assert "Cost Saved" in content
    assert "4,800" in content
    assert "$0.0007" in content
    assert "0.20%" in content  # visual change percentage


# ------------------------------------------------------------------------------
# Driver Loop Integration with pHash Gating and ROI Cropping Tests
# ------------------------------------------------------------------------------


@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
@patch.object(ReachDriver, "execute_action")
def test_driver_loop_skips_vlm_on_subthreshold_after_wait(
    mock_exec: MagicMock,
    mock_agy: MagicMock,
    mock_text: MagicMock,
    mock_shot: MagicMock,
    tmp_path: Path,
):
    frame = make_test_png(40, 40, b"\x20\x20\x20")
    shot_path = tmp_path / "screen.png"
    shot_path.write_bytes(frame)
    mock_shot.return_value = str(shot_path)
    mock_text.return_value = "Normal page"
    mock_exec.return_value = {"status": "ok"}

    # Model proposals:
    # Step 1: proposes wait
    # Step 2: (change gate intercepts identical frame, agy is NOT called!)
    # Step 3: (change gate intercepts identical frame, agy is NOT called!)
    # Step 4: proposes terminate
    mock_agy.side_effect = [
        json.dumps({
            "status": "SUCCESS",
            "response": json.dumps({"action": {"kind": "wait", "description": "Wait for data"}}),
        }),
        json.dumps({
            "status": "SUCCESS",
            "response": json.dumps({"action": {"kind": "terminate", "description": "Done after wait"}}),
        }),
    ]

    driver = ReachDriver(
        api_url="http://127.0.0.1:4200",
        audit_dir=tmp_path / "audit",
        max_steps=5,
        min_change_threshold=0.01,
        max_unchanged_ticks=2,  # Max 2 skipped ticks before force
        backoff_sec=0.01,
    )

    with patch("time.sleep"):
        res = driver.drive(goal="Wait for data settle")

    assert res.success is True
    assert res.status == "completed"
    assert res.skipped_vlm_ticks == 2
    assert res.tokens_saved == 2 * ESTIMATED_TOKENS_PER_VLM_CALL
    # Check that steps were marked vlm_cached
    cached_steps = [s for s in res.steps if s.vlm_cached]
    assert len(cached_steps) == 2
    for s in cached_steps:
        assert s.action.kind == "wait"
        assert s.result.get("status") == "vlm_cached"
