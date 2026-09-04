"""Unit tests for Dangerous Mutation Approval Gate in Reach CUA Driver."""

import json
from pathlib import Path
import sys
from unittest.mock import MagicMock, patch
import pytest

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_drive import (
    ApprovalGate,
    ReachAction,
    ReachDriver,
    DEFAULT_MUTATION_PATTERNS,
)


def test_dangerous_pattern_detection() -> None:
    gate = ApprovalGate()

    dangerous_cases = [
        ReachAction(kind="click", target="Delete Account Button", description="Click delete"),
        ReachAction(kind="click", target="Remove Team Member", description="Remove user"),
        ReachAction(kind="click", target="Confirm Payment", description="Submit payment $100"),
        ReachAction(kind="click", target="Pay Now", description="Pay for order"),
        ReachAction(kind="click", target="Purchase License", description="Complete purchase"),
        ReachAction(kind="click", target="Transfer Funds", description="Send wire transfer"),
        ReachAction(kind="type", target="SQL Console", value="DROP TABLE users;", description="Execute query"),
        ReachAction(kind="click", target="Terminate Account", description="Close account"),
        ReachAction(kind="click", target="Cancel Subscription", description="Cancel billing plan"),
        ReachAction(kind="type", target="Terminal", value="rm -rf /workspace", description="Wipe files"),
        ReachAction(kind="click", action_class="dangerous", target="Custom Button", description="Custom dangerous"),
    ]

    for action in dangerous_cases:
        is_dangerous, reason = gate.check_action(action)
        assert is_dangerous, f"Expected {action} to be flagged as dangerous"
        assert reason is not None


def test_safe_actions_pass() -> None:
    gate = ApprovalGate()

    safe_cases = [
        ReachAction(kind="click", target="Next Page", description="Go to page 2"),
        ReachAction(kind="click", target="View Profile", description="Open profile"),
        ReachAction(kind="type", target="Search Box", value="reach documentation", description="Search docs"),
        ReachAction(kind="key", key="Return", description="Press Enter"),
        ReachAction(kind="navigate", target="https://example.com", description="Browse home"),
    ]

    for action in safe_cases:
        is_dangerous, reason = gate.check_action(action)
        assert not is_dangerous, f"Expected {action} to be safe, but flagged: {reason}"
        assert reason is None


def test_allow_mutations_flag_bypasses_pause() -> None:
    gate = ApprovalGate(allow_mutations=True)
    action = ReachAction(kind="click", target="Delete Database", description="Drop all tables")

    is_dangerous, reason, approved = gate.evaluate(action)
    assert is_dangerous is True
    assert approved is True
    assert action.requires_approval is True
    assert action.action_class == "REQUIRES_APPROVAL"


def test_approval_callback() -> None:
    # Callback approves
    gate_approve = ApprovalGate(approval_callback=lambda act, reason: True)
    action1 = ReachAction(kind="click", target="Pay Invoice")
    _, _, approved1 = gate_approve.evaluate(action1)
    assert approved1 is True

    # Callback rejects
    gate_reject = ApprovalGate(approval_callback=lambda act, reason: False)
    action2 = ReachAction(kind="click", target="Pay Invoice")
    _, _, approved2 = gate_reject.evaluate(action2)
    assert approved2 is False


def test_non_interactive_mode_requires_approval(capsys: pytest.CaptureFixture) -> None:
    gate = ApprovalGate(interactive=False)
    action = ReachAction(kind="click", target="Delete Cluster")

    is_dangerous, reason, approved = gate.evaluate(action)
    assert is_dangerous is True
    assert approved is False
    assert "delete" in reason.lower()


def test_interactive_prompt_approved(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("sys.stdin.readline", lambda: "y\n")
    gate = ApprovalGate(interactive=True)
    action = ReachAction(kind="click", target="Confirm Payment")

    is_dangerous, reason, approved = gate.evaluate(action)
    assert is_dangerous is True
    assert approved is True


def test_interactive_prompt_rejected(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("sys.stdin.readline", lambda: "n\n")
    gate = ApprovalGate(interactive=True)
    action = ReachAction(kind="click", target="Terminate Account")

    is_dangerous, reason, approved = gate.evaluate(action)
    assert is_dangerous is True
    assert approved is False


@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
@patch.object(ReachDriver, "execute_action")
def test_driver_pauses_on_dangerous_action_without_execution(
    mock_exec: MagicMock,
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

    driver = ReachDriver(
        api_url="http://127.0.0.1:4200",
        interactive=False,  # Non-interactive mode
        allow_mutations=False,
    )

    result = driver.drive(goal="Clean up organization")
    assert result.success is False
    assert result.status == "approval_required"
    assert "requires approval" in result.final_description.lower()

    # Ensure dangerous action was NEVER executed!
    mock_exec.assert_not_called()
    assert len(result.steps) == 1
    assert result.steps[0].action.requires_approval is True
    assert result.steps[0].result["status"] == "approval_required"


@patch.object(ReachDriver, "capture_screenshot")
@patch.object(ReachDriver, "capture_page_text")
@patch.object(ReachDriver, "invoke_agy")
@patch.object(ReachDriver, "execute_action")
def test_driver_executes_dangerous_action_when_allow_mutations_set(
    mock_exec: MagicMock,
    mock_agy: MagicMock,
    mock_text: MagicMock,
    mock_shot: MagicMock,
) -> None:
    mock_shot.return_value = "/tmp/dummy.png"
    mock_text.return_value = "Billing page"
    mock_exec.return_value = {"status": "ok"}
    # Step 1 proposes payment, Step 2 terminates
    mock_agy.side_effect = [
        json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {
                        "action": {
                            "kind": "click",
                            "target": "Pay $25 invoice",
                            "point": [200, 300],
                            "description": "Authorize charge",
                        }
                    }
                ),
            }
        ),
        json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {"action": {"kind": "terminate", "description": "Payment completed"}}
                ),
            }
        ),
    ]

    driver = ReachDriver(
        api_url="http://127.0.0.1:4200",
        allow_mutations=True,  # Explicitly allow mutations
        interactive=False,
    )

    result = driver.drive(goal="Pay pending bill")
    assert result.success is True
    assert result.status == "completed"

    # Action was executed because allow_mutations=True was passed
    mock_exec.assert_called_once()
    assert mock_exec.call_args[0][0].target == "Pay $25 invoice"
