#!/usr/bin/env python3
"""Measure local AXI observation representations without contacting live sites.

The benchmark treats observations as supplied local/disposable data.  It measures
UTF-8 bytes and local transformation latency, reports token *estimates* plainly,
and checks that expected accessibility references survive each representation.
"""

from __future__ import annotations

import argparse
import json
import math
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple
from urllib.parse import urlparse

DEFAULT_TARGET = "loopback://fixture"
OBSERVATION_MODES: Tuple[str, ...] = ("full", "compact", "query")


@dataclass(frozen=True)
class ObservationCase:
    name: str
    observations: Mapping[str, Any]
    expected_refs: Tuple[str, ...] = ()


def _validate_target(target: str) -> str:
    """Allow only loopback/disposable targets; never benchmark a real site."""
    parsed = urlparse(target)
    if target == DEFAULT_TARGET or parsed.scheme == "loopback":
        return target
    host = (parsed.hostname or "").lower()
    if parsed.scheme in {"http", "https"} and host in {"localhost", "127.0.0.1", "::1"}:
        return target
    raise ValueError("benchmark target must be loopback://fixture or a loopback URL")


def _default_cases() -> List[ObservationCase]:
    return [
        ObservationCase(
            name="fixture-login",
            observations={
                "full": {
                    "text": "Sign in",
                    "refs": ["@e1", "@e2"],
                    "elements": [{"ref": "@e1", "role": "button", "name": "Sign in"}],
                },
                "compact": {"text": "Sign in", "refs": ["@e1"]},
                "query": {"text": "Sign in", "refs": ["@e1"]},
            },
            expected_refs=("@e1",),
        )
    ]


def _coerce_case(raw: Mapping[str, Any], index: int) -> ObservationCase:
    name = str(raw.get("name") or f"case-{index + 1}")
    observations: Dict[str, Any] = {}
    for mode in OBSERVATION_MODES:
        value = raw.get(mode, raw.get(f"{mode}_observation"))
        if value is None and isinstance(raw.get("observations"), Mapping):
            nested = raw["observations"]
            value = nested.get(mode, nested.get(f"{mode}_observation"))
        if value is not None:
            observations[mode] = value
    if not observations:
        raise ValueError(f"case {name!r} has no full, compact, or query observation")
    expected = raw.get("expected_refs", raw.get("expected_ref", []))
    if isinstance(expected, str):
        expected_refs = (expected,)
    elif isinstance(expected, Sequence):
        expected_refs = tuple(str(ref) for ref in expected)
    else:
        raise ValueError(f"case {name!r} expected_refs must be a string or list")
    return ObservationCase(name=name, observations=observations, expected_refs=expected_refs)


def load_cases(path: Optional[str] = None) -> List[ObservationCase]:
    if path is None:
        return _default_cases()
    payload = json.loads(Path(path).read_text(encoding="utf-8"))
    rows = payload.get("cases") if isinstance(payload, Mapping) else payload
    if not isinstance(rows, list):
        raise ValueError("cases file must contain a JSON list or {\"cases\": [...]}")
    return [_coerce_case(row, index) for index, row in enumerate(rows) if isinstance(row, Mapping)]


def _refs_from_observation(observation: Any) -> Tuple[str, ...]:
    if isinstance(observation, Mapping):
        refs = observation.get("refs", observation.get("expected_refs", []))
        if isinstance(refs, str):
            return (refs,)
        if isinstance(refs, Sequence):
            return tuple(str(ref) for ref in refs)
    return ()


def _serialize_observation(observation: Any) -> bytes:
    if isinstance(observation, bytes):
        return observation
    if isinstance(observation, str):
        return observation.encode("utf-8")
    return json.dumps(observation, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def measure_observation(case: ObservationCase, mode: str) -> Dict[str, Any]:
    if mode not in OBSERVATION_MODES:
        raise ValueError(f"unknown observation mode: {mode}")
    if mode not in case.observations:
        return {
            "case": case.name,
            "mode": mode,
            "available": False,
            "utf8_bytes": None,
            "latency_seconds": None,
            "token_estimate": None,
            "expected_refs": list(case.expected_refs),
            "retained_expected_refs": [],
            "missing_expected_refs": list(case.expected_refs),
            "ref_fidelity": None,
        }

    started = time.perf_counter()
    encoded = _serialize_observation(case.observations[mode])
    observed_refs = set(_refs_from_observation(case.observations[mode]))
    retained = [ref for ref in case.expected_refs if ref in observed_refs]
    missing = [ref for ref in case.expected_refs if ref not in observed_refs]
    elapsed = time.perf_counter() - started
    return {
        "case": case.name,
        "mode": mode,
        "available": True,
        "utf8_bytes": len(encoded),
        "latency_seconds": elapsed,
        "token_estimate": {
            "value": int(math.ceil(len(encoded) / 4)) if encoded else 0,
            "label": "estimate only (UTF-8 bytes / 4); provider tokens not measured",
        },
        "expected_refs": list(case.expected_refs),
        "retained_expected_refs": retained,
        "missing_expected_refs": missing,
        "ref_fidelity": (len(retained) / len(case.expected_refs)) if case.expected_refs else None,
    }


def run_benchmark(
    cases: Iterable[ObservationCase],
    *,
    target: str = DEFAULT_TARGET,
    modes: Sequence[str] = OBSERVATION_MODES,
) -> Dict[str, Any]:
    target = _validate_target(target)
    normalized_modes = tuple(modes)
    for mode in normalized_modes:
        if mode not in OBSERVATION_MODES:
            raise ValueError(f"unknown observation mode: {mode}")
    measurements = [
        measure_observation(case, mode)
        for case in cases
        for mode in normalized_modes
    ]
    return {
        "target": target,
        "target_policy": "loopback/disposable only; no live-site calls",
        "measurements": measurements,
        "measurement_notes": {
            "latency": "local serialization and reference inspection wall-clock seconds",
            "bytes": "measured UTF-8 encoded observation bytes",
            "tokens": "explicit estimates only; no model/provider version inferred",
            "ref_fidelity": "retained expected refs divided by expected refs",
        },
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default=DEFAULT_TARGET, help="loopback/disposable target identifier")
    parser.add_argument("--cases", help="local JSON cases file; defaults to the loopback fixture")
    parser.add_argument(
        "--mode",
        action="append",
        choices=OBSERVATION_MODES,
        dest="modes",
        help="observation mode to measure (repeatable; defaults to all modes)",
    )
    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    result = run_benchmark(
        load_cases(args.cases),
        target=args.target,
        modes=tuple(args.modes or OBSERVATION_MODES),
    )
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
