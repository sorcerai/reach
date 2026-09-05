#!/usr/bin/env python3
"""Run Browser Use agent missions on Agent Computer virtual screens.

Connects to the isolated virtual display via Chrome DevTools Protocol (CDP),
allowing live human viewing via noVNC while browser-use executes DOM/vision tasks.

Usage:
  python3 scripts/reach_browser_use.py --task "Search Hacker News for AI articles" --screen 0
  python3 scripts/reach_browser_use.py --task "Check weather in Tokyo" --cdp-url http://127.0.0.1:9222
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
from pathlib import Path
import sys

# Ensure repository root is on sys.path
REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from integrations.browser_use import AgentComputerBrowserAdapter  # noqa: E402

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)
logger = logging.getLogger("reach_browser_use")


async def run_mission(
    task: str,
    screen: int = 0,
    api_url: str = "http://127.0.0.1:4200",
    cdp_url: str | None = None,
    model_name: str = "gemini-2.0-flash",
    max_steps: int = 20,
) -> dict:
    """Execute a Browser Use mission on the designated screen."""
    try:
        from browser_use import Agent, Browser, BrowserConfig  # type: ignore
    except ImportError:
        logger.error(
            "The 'browser-use' library is not installed.\n"
            "Install it via:\n"
            "  pip install browser-use\n"
            "or\n"
            "  uv pip install browser-use"
        )
        return {
            "status": "error",
            "error": "browser-use library missing",
        }

    # Initialize adapter
    adapter = AgentComputerBrowserAdapter(
        screen_id=screen,
        api_url=api_url,
    )
    resolved_cdp = cdp_url or adapter.cdp_url

    logger.info(f"Connecting browser-use to Agent Computer screen {screen} at {resolved_cdp}")
    logger.info(f"Live noVNC observation URL: {adapter.novnc_url}")

    # Lease the virtual screen
    adapter.lease_screen()

    try:
        browser = Browser(config=BrowserConfig(cdp_url=resolved_cdp))

        # Check for LangChain model initialization
        llm = None
        if "GEMINI_API_KEY" in os.environ or "GOOGLE_API_KEY" in os.environ:
            try:
                from langchain_google_genai import ChatGoogleGenerativeAI  # type: ignore

                api_key = os.environ.get("GEMINI_API_KEY") or os.environ.get("GOOGLE_API_KEY")
                llm = ChatGoogleGenerativeAI(model=model_name, google_api_key=api_key)
            except ImportError:
                pass

        if llm is None and "OPENAI_API_KEY" in os.environ:
            try:
                from langchain_openai import ChatOpenAI  # type: ignore

                llm = ChatOpenAI(model="gpt-4o")
            except ImportError:
                pass

        agent_kwargs = {
            "task": task,
            "browser": browser,
            "max_steps": max_steps,
        }
        if llm is not None:
            agent_kwargs["llm"] = llm

        agent = Agent(**agent_kwargs)
        history = await agent.run()

        logger.info(f"Mission complete. Steps executed: {len(history)}")
        return {
            "status": "success",
            "screen": screen,
            "task": task,
            "novnc_url": adapter.novnc_url,
            "steps": len(history),
        }
    except Exception as e:
        logger.error(f"Mission execution failed: {e}", exc_info=True)
        return {
            "status": "failed",
            "screen": screen,
            "error": str(e),
        }
    finally:
        adapter.release_screen()


def main() -> None:
    parser = argparse.ArgumentParser(description="Run Browser Use agent on Agent Computer screens")
    parser.add_argument("--task", "-t", required=True, help="Task description for the agent")
    parser.add_argument("--screen", "-s", type=int, default=0, help="Screen index (0..3, default: 0)")
    parser.add_argument("--cdp-url", help="Override CDP endpoint URL (e.g. http://127.0.0.1:9222)")
    parser.add_argument("--api-url", default="http://127.0.0.1:4200", help="Agent Computer API URL")
    parser.add_argument("--model", default="gemini-2.0-flash", help="LLM model name")
    parser.add_argument("--max-steps", type=int, default=20, help="Maximum action steps")
    parser.add_argument("--json", action="store_true", help="Output results in JSON format")

    args = parser.parse_args()

    result = asyncio.run(
        run_mission(
            task=args.task,
            screen=args.screen,
            api_url=args.api_url,
            cdp_url=args.cdp_url,
            model_name=args.model,
            max_steps=args.max_steps,
        )
    )

    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(f"Status: {result.get('status')}")
        if result.get("status") == "success":
            print(f"Executed on screen: {result.get('screen')}")
            print(f"View live at: {result.get('novnc_url')}")
        elif "error" in result:
            print(f"Error: {result.get('error')}", file=sys.stderr)


if __name__ == "__main__":
    main()
