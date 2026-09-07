"""Security tests for server-owned mutation approval."""

import json
from pathlib import Path
import sys
from unittest.mock import MagicMock, patch
import pytest

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_drive import (
    ApprovalRequiredError,
    ReachDriver,
    UncertainMutationError,
)

@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
def test_driver_stops_when_server_requires_approval(
    mock_agy: MagicMock,
    mock_text: MagicMock,
    mock_shot: MagicMock,
    capsys: pytest.CaptureFixture,
) -> None:
    mock_shot.return_value = "/tmp/dummy.png"
    mock_text.return_value = "Settings page"
    mock_agy.return_value = json.dumps(
        {
            "status": "SUCCESS",
            "response": json.dumps(
                {
                    "action": {
                        "kind": "click",
                        "target": "Delete entire organization",
                        "point": [350, 600],
                        "description": "Click delete button to purge data",
                    }
                }
            ),
        }
    )

    driver = ReachDriver(api_url="http://127.0.0.1:4200")
    driver.call_mcp_tool = MagicMock(
        side_effect=ApprovalRequiredError("approval-digest")
    )

    result = driver.drive(goal="Clean up organization")
    assert result.success is False
    assert result.status == "approval_required"
    assert "without replay" in result.final_description.lower()
    assert len(result.steps) == 1
    assert result.steps[-1].result["digest"] == "approval-digest"
    assert "[APPROVAL GATE]" not in capsys.readouterr().err

@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
def test_driver_stops_when_mutation_outcome_is_uncertain(
    mock_agy: MagicMock,
    mock_text: MagicMock,
    mock_shot: MagicMock,
) -> None:
    mock_shot.return_value = "/tmp/dummy.png"
    mock_text.return_value = "Settings page"
    mock_agy.return_value = json.dumps(
        {
            "status": "SUCCESS",
            "response": json.dumps(
                {
                    "action": {
                        "kind": "click",
                        "target": "Delete entire organization",
                        "point": [350, 600],
                        "description": "Click delete button to purge data",
                    }
                }
            ),
        }
    )

    driver = ReachDriver(api_url="http://127.0.0.1:4200")
    driver.call_mcp_tool = MagicMock(
        side_effect=UncertainMutationError(
            "transport failure during mutation; outcome is uncertain"
        )
    )

    result = driver.drive(goal="Clean up organization")
    assert result.success is False
    assert result.status == "uncertain"
    assert "without replay" in result.final_description.lower()
    assert len(result.steps) == 1
    assert result.steps[-1].result["status"] == "uncertain"
