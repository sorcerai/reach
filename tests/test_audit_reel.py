"""Unit tests for Visual Diff Audit Reel and HTML report generation."""

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
    ReachAction,
    ReachDriver,
    StepRecord,
    generate_html_report,
)


def test_generate_html_report_structure(tmp_path: Path) -> None:
    audit_dir = tmp_path / "task_test_001"
    audit_dir.mkdir(parents=True, exist_ok=True)

    # Create dummy before and after images
    before_img = audit_dir / "step_001_before.png"
    after_img = audit_dir / "step_001_after.png"
    before_img.write_bytes(b"\x89PNG\r\n\x1a\nfakebefore")
    after_img.write_bytes(b"\x89PNG\r\n\x1a\nfakeafter")

    meta = {
        "task_id": "task_20260904_120000_abc123",
        "goal": "Verify payment confirmation <script>alert(1)</script>",
        "screen": 0,
        "model": "gemini-3.8-flash-high",
        "status": "completed",
        "success": True,
        "final_description": "Payment confirmed",
        "start_time": "2026-09-04T12:00:00Z",
        "end_time": "2026-09-04T12:00:05Z",
        "duration_sec": 5.25,
        "steps": [
            {
                "step_index": 1,
                "action": {
                    "kind": "click",
                    "point": [420, 680],
                    "target": "Confirm Button",
                    "description": "Click submit button",
                    "action_class": "read_only",
                },
                "observation_summary": "Order summary page ready",
                "timestamp": "2026-09-04T12:00:01Z",
                "result": {"status": "ok"},
            }
        ],
    }

    report_path = generate_html_report(audit_dir, meta)
    assert os.path.isfile(report_path)
    assert report_path.endswith("report.html")

    with open(report_path, "r", encoding="utf-8") as f:
        html_content = f.read()

    # Checks
    assert "task_20260904_120000_abc123" in html_content
    assert "COMPLETED" in html_content
    assert "5.25s" in html_content
    assert "Step #1" in html_content
    assert "CLICK" in html_content
    assert "Confirm Button" in html_content
    assert "step_001_before.png" in html_content
    assert "step_001_after.png" in html_content
    assert "420px" in html_content  # Click indicator coordinate
    assert "680px" in html_content  # Click indicator coordinate
    # HTML escaping check
    assert "<script>alert(1)</script>" not in html_content
    assert "&lt;script&gt;alert(1)&lt;/script&gt;" in html_content


@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
@patch.object(ReachDriver, "execute_action")
def test_driver_records_audit_reel_and_creates_report(
    mock_exec: MagicMock,
    mock_agy: MagicMock,
    mock_text: MagicMock,
    mock_shot: MagicMock,
    tmp_path: Path,
) -> None:
    audit_dir = tmp_path / "custom_audit"
    fake_screenshot = tmp_path / "temp_screen.png"
    fake_screenshot.write_bytes(b"\x89PNG\r\n\x1a\nshot")
    mock_shot.return_value = str(fake_screenshot)
    mock_text.return_value = "Home page"
    mock_exec.return_value = {"status": "ok"}

    mock_agy.side_effect = [
        json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {
                        "action": {
                            "kind": "click",
                            "point": [100, 200],
                            "target": "Catalog link",
                            "description": "Open catalog",
                        }
                    }
                ),
            }
        ),
        json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {"action": {"kind": "terminate", "description": "Catalog displayed"}}
                ),
            }
        ),
    ]

    driver = ReachDriver(
        api_url="http://127.0.0.1:4200",
        task_id="test_task_xyz",
        audit_dir=audit_dir,
        enable_audit=True,
    )

    result = driver.drive(goal="Browse catalog items")

    assert result.success is True
    assert result.status == "completed"
    assert result.audit_report_path is not None
    assert os.path.isfile(result.audit_report_path)

    # Check audit directory contents
    assert (audit_dir / "audit_meta.json").exists()
    assert (audit_dir / "report.html").exists()
    assert (audit_dir / "step_001_before.png").exists()

    with open(audit_dir / "audit_meta.json", "r", encoding="utf-8") as f:
        meta_data = json.load(f)

    assert meta_data["task_id"] == "test_task_xyz"
    assert meta_data["goal"] == "Browse catalog items"
    assert meta_data["status"] == "completed"
    assert len(meta_data["steps"]) == 2
    assert meta_data["steps"][0]["action"]["kind"] == "click"


@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
def test_driver_with_audit_disabled(
    mock_agy: MagicMock,
    mock_text: MagicMock,
    mock_shot: MagicMock,
    tmp_path: Path,
) -> None:
    audit_dir = tmp_path / "should_not_exist"
    mock_shot.return_value = "/tmp/dummy.png"
    mock_text.return_value = "Page"
    mock_agy.return_value = json.dumps(
        {
            "status": "SUCCESS",
            "response": json.dumps(
                {"action": {"kind": "terminate", "description": "Done immediately"}}
            ),
        }
    )

    driver = ReachDriver(
        api_url="http://127.0.0.1:4200",
        audit_dir=audit_dir,
        enable_audit=False,
    )

    result = driver.drive(goal="No audit goal")
    assert result.success is True
    assert result.audit_report_path is None
    assert not audit_dir.exists()
