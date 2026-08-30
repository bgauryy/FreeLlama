#!/usr/bin/env python3
"""Build a dated judge-calibration artifact from human-labeled comparison records."""

from __future__ import annotations

import argparse
from datetime import date, timedelta
import json
from pathlib import Path
from typing import Any

from _common import load_json, write_json
from _schema import require_valid


def weighted_kappa(human: list[int], judge: list[int], maximum: int = 5) -> float:
    if len(human) != len(judge) or not human:
        return 0.0
    observed = sum(((left - right) / maximum) ** 2 for left, right in zip(human, judge)) / len(human)
    human_counts = [human.count(value) / len(human) for value in range(maximum + 1)]
    judge_counts = [judge.count(value) / len(judge) for value in range(maximum + 1)]
    expected = sum(human_counts[left] * judge_counts[right] * ((left - right) / maximum) ** 2 for left in range(maximum + 1) for right in range(maximum + 1))
    return 1.0 if expected == 0 and observed == 0 else (0.0 if expected == 0 else 1 - observed / expected)


def main() -> int:
    parser = argparse.ArgumentParser(description="Calibrate a judge against human-labeled pairwise and rubric records.")
    parser.add_argument("--input", required=True, type=Path, help="JSON object with cases[].")
    parser.add_argument("--judge-model", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--checked-at", default=date.today().isoformat())
    parser.add_argument("--valid-days", type=int, default=30)
    args = parser.parse_args()
    checked = date.fromisoformat(args.checked_at)
    if args.valid_days < 1: raise SystemExit("--valid-days must be positive")
    cases: list[dict[str, Any]] = load_json(args.input).get("cases", [])
    winners = {"A", "B", "TIE"}
    if any(case.get("human_winner") not in winners or case.get("judge_winner") not in winners or case.get("swapped_judge_winner") not in winners for case in cases):
        raise SystemExit("every case needs human_winner, judge_winner, and swapped_judge_winner in A|B|TIE")
    if any(not isinstance(case.get("human_score"), int) or not 0 <= case["human_score"] <= 5 or not isinstance(case.get("judge_score"), int) or not 0 <= case["judge_score"] <= 5 for case in cases):
        raise SystemExit("every case needs integer human_score and judge_score from 0 to 5")
    count = len(cases)
    agreement = sum(case["human_winner"] == case["judge_winner"] for case in cases) / count if count else 0.0
    swap = sum(case["judge_winner"] == case["swapped_judge_winner"] for case in cases) / count if count else 0.0
    kappa = weighted_kappa([case["human_score"] for case in cases], [case["judge_score"] for case in cases])
    gates = {"minimum_cases": 20, "pairwise_agreement": 0.85, "weighted_kappa": 0.70, "order_swap_consistency": 0.95}
    passed = count >= gates["minimum_cases"] and agreement >= gates["pairwise_agreement"] and kappa >= gates["weighted_kappa"] and swap >= gates["order_swap_consistency"]
    artifact = {
        "schema_version": 1, "judge_model": args.judge_model, "checked_at": checked.isoformat(), "expires_at": (checked + timedelta(days=args.valid_days)).isoformat(), "case_count": count,
        "metrics": {"pairwise_agreement": round(agreement, 6), "weighted_kappa": round(kappa, 6), "order_swap_consistency": round(swap, 6)}, "gates": gates, "passed": passed,
        "source": str(args.input),
    }
    require_valid(artifact, load_json(Path(__file__).resolve().parent.parent / "schemas/judge-calibration.schema.json"), "calibration")
    write_json(args.output, artifact)
    print(json.dumps(artifact, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
