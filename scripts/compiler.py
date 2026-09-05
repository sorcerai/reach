#!/usr/bin/env python3
"""Reach Routine Compiler module.

Exposes RoutineCompiler, Checkpoint, CompiledAction, CompiledStep, and CompiledRoutine.
Can be run directly or imported as `from scripts.compiler import RoutineCompiler`.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Ensure repo root is on sys.path
REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_routine import (
    Checkpoint,
    CompiledAction,
    CompiledRoutine,
    CompiledStep,
    RoutineCompiler,
    RoutineTrace,
    TraceStep,
    compute_frame_hash_hex,
    hash_distance,
    render_template,
    resolve_routine_dir,
)

__all__ = [
    "RoutineCompiler",
    "CompiledRoutine",
    "CompiledStep",
    "CompiledAction",
    "Checkpoint",
    "RoutineTrace",
    "TraceStep",
    "render_template",
    "compute_frame_hash_hex",
    "hash_distance",
    "resolve_routine_dir",
]


def main() -> None:
    parser = argparse.ArgumentParser(description="Compile a Reach routine trace into routine.json")
    parser.add_argument("name", help="Routine name or path to trace.json")
    parser.add_argument("--params", default=None, help="JSON parameter mapping, e.g. '{\"Tesla\": \"query\"}'")
    parser.add_argument("--routines-dir", default=None, help="Custom base routines directory")
    args = parser.parse_args()

    param_map = json.loads(args.params) if args.params else None
    compiler = RoutineCompiler()
    routine = compiler.compile(args.name, parameter_mappings=param_map, routines_dir=args.routines_dir)
    print(f"[✓] Compiled routine '{routine.name}' (v{routine.version})")
    print(f"    Parameters: {routine.parameters}")
    print(f"    Steps count: {len(routine.steps)}")


if __name__ == "__main__":
    main()
