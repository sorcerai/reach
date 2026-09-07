"""Security tests for metadata-only audit persistence."""

import json
import os
from pathlib import Path
import sys
from unittest.mock import MagicMock, patch


REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_drive import ReachDriver


@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
@patch.object(ReachDriver, "execute_action")
def test_driver_audit_persists_redacted_metadata_only(
    mock_exec: MagicMock,
    mock_agy: MagicMock,
    mock_text: MagicMock,
    mock_shot: MagicMock,
    tmp_path: Path,
) -> None:
    audit_dir = tmp_path / "audit"
    fake_screenshot = tmp_path / "live.png"
    fake_screenshot.write_bytes(b"\x89PNG\r\n\x1a\nlive")
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
                            "kind": "type",
                            "target": "Password field",
                            "value": "typed-secret-canary",
                            "description": "Type typed-secret-canary",
                        }
                    }
                ),
            }
        ),
        json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {
                        "action": {
                            "kind": "terminate",
                            "outcome": "completed",
                            "description": "Finished with typed-secret-canary",
                        }
                    }
                ),
            }
        ),
    ]

    driver = ReachDriver(
        api_url="http://127.0.0.1:4200",
        task_id="audit-security-test",
        audit_dir=audit_dir,
        enable_audit=True,
        completion_text="Home page",
    )
    result = driver.drive(goal="goal-canary typed-secret-canary")

    assert result.success is True
    assert result.status == "completed"
    assert result.audit_report_path is not None
    assert os.path.isfile(result.audit_report_path)

    report_path = Path(result.audit_report_path)
    assert report_path.resolve().is_relative_to(audit_dir.resolve())
    meta_path = report_path.parent / "audit_meta.json"
    meta_text = meta_path.read_text(encoding="utf-8")
    report_text = report_path.read_text(encoding="utf-8")
    for persisted in (meta_text, report_text):
        assert "goal-canary" not in persisted
        assert "typed-secret-canary" not in persisted
    assert "goal" not in json.loads(meta_text)
    assert not list(audit_dir.rglob("*.png"))


@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
def test_driver_with_audit_disabled_writes_no_audit_files(
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
                {
                    "action": {
                        "kind": "terminate",
                        "outcome": "completed",
                        "description": "Done immediately",
                    }
                }
            ),
        }
    )

    driver = ReachDriver(
        api_url="http://127.0.0.1:4200",
        audit_dir=audit_dir,
        enable_audit=False,
        completion_text="Page",
    )

    result = driver.drive(goal="No audit goal")
    assert result.success is True
    assert result.audit_report_path is None
    assert not audit_dir.exists()
