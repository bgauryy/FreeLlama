#!/usr/bin/env bash
# Runs the octocode-vs-bash matrix over the 30-question suite for one target model, then
# aggregates, renders, and logs the run to runs/index.jsonl.
#
# Usage: ./run_all.sh [--model <ollama-tag>] [--trials N] [extra run_matrix.py flags]
#
# Swapping models is a one-flag change — nothing to hand-edit:
#   ./run_all.sh --model qwen3.8:27b-mlx
#   ./run_all.sh --model qwen2.5:7b
# Each model gets its own results/<model-slug>/ directory (never overwritten by a different
# model) and one line appended to runs/index.jsonl recording the run_id(s), model, date, and
# where the data lives.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HARNESS="$(cd "$HERE/../harness/scripts" && pwd)"

MODEL="qwen3.8:27b-mlx"
TRIALS=1
EXTRA_ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --model) MODEL="$2"; shift 2 ;;
    --trials) TRIALS="$2"; shift 2 ;;
    *) EXTRA_ARGS+=("$1"); shift ;;
  esac
done

# Filesystem-safe slug: qwen3.8:27b-mlx -> qwen3.8-27b-mlx
MODEL_SLUG="$(echo "$MODEL" | tr ':/' '-')"

GENERATED_DIR="$HERE/tasks/.generated"
mkdir -p "$GENERATED_DIR"
MATRIX_FILE="$GENERATED_DIR/matrix-$MODEL_SLUG.json"
sed -e "s/__MODEL_SLUG__/$MODEL_SLUG/g" -e "s/__MODEL__/$MODEL/g" \
  "$HERE/tasks/octocode-vs-bash-matrix.template.json" > "$MATRIX_FILE"

RESULTS_DIR="$HERE/results/$MODEL_SLUG"
mkdir -p "$RESULTS_DIR"

echo "model:   $MODEL"
echo "matrix:  $MATRIX_FILE"
echo "results: $RESULTS_DIR"
echo

python3 "$HARNESS/run_matrix.py" \
  --matrix "$MATRIX_FILE" \
  --suite "$HERE/tasks/octocode-vs-bash-30.json" \
  --results "$RESULTS_DIR" \
  --trials "$TRIALS" \
  --continue-on-error \
  --discard-workspaces \
  "${EXTRA_ARGS[@]}"

echo
echo "dashboard: $RESULTS_DIR/index.html"
echo "aggregate: $RESULTS_DIR/aggregate.json"

# Log this run to a durable, tracked ledger: runId + model + date + where the data is.
python3 - "$MODEL" "$MODEL_SLUG" "$RESULTS_DIR" "$TRIALS" "$HERE" <<'PYEOF'
import json, sys
from datetime import datetime, timezone
from pathlib import Path

model, model_slug, results_dir, trials, here = sys.argv[1:6]
results_dir = Path(results_dir)
aggregate_path = results_dir / "aggregate.json"
entry = {
    "logged_at": datetime.now(timezone.utc).isoformat(),
    "model": model,
    "model_slug": model_slug,
    "trials": int(trials),
    "results_dir": str(results_dir.relative_to(here)),
    "runs": [],
}
if aggregate_path.is_file():
    aggregate = json.loads(aggregate_path.read_text())
    for m in aggregate.get("models", []):
        entry["runs"].append({
            "agent_id": m.get("id"),
            "agent": m.get("agent"),
            "run_id": m.get("run_id"),
            "benchmark_date": m.get("benchmark_date"),
            "deterministic_pass_rate": m.get("deterministic_pass_rate"),
            "coverage": m.get("coverage", {}).get("rate"),
        })
ledger = Path(here) / "runs" / "index.jsonl"
ledger.parent.mkdir(parents=True, exist_ok=True)
with ledger.open("a", encoding="utf-8") as f:
    f.write(json.dumps(entry, ensure_ascii=False) + "\n")
print(f"logged run to {ledger.relative_to(here) if ledger.is_relative_to(here) else ledger}")
PYEOF
