#!/usr/bin/env bash
# Local RAG in ~40 lines, using FreeLlama only for the embedding transform.
#
# FreeLlama deliberately owns no vector store — persistence is a standing non-goal, and a stale
# index fails silently, returning confidently wrong files as the corpus drifts. So the pattern is:
# FreeLlama turns text into vectors, YOU own storage and staleness. Swap the jq/cosine step below
# for sqlite-vec, LanceDB, or chroma the moment the corpus outgrows a flat file.
#
# Before reaching for this: for code, `grep` beat embedding search here on accuracy, latency and
# cost at once. Use embeddings when there is no keyword to search for.
#
# MEASURED on this repo (242 chunks from packages/rust-core/src), top-1 retrieval:
#   "retry with exponential backoff on transient failure"   -> proxy.rs   correct
#   "turn a benchmark aggregate into a routing policy"      -> policy.rs  correct
#   "how does it avoid loading two models into memory"      -> lib.rs     WRONG (want platform.rs)
#
# That third question has now failed twice, in two separate runs, and the reason is the useful
# part: platform.rs never says "loading two models". It says `managed_execution`, `RwLock`,
# "admission permit". Embeddings match how text is PHRASED, and code routinely expresses a concept
# in vocabulary that looks nothing like how a person asks about it. When you can guess the
# identifier, grep finds it instantly and exactly; embeddings are for when you cannot.
#
# Usage:  ./local-rag.sh index <dir>       build corpus.txt + vectors.json
#         ./local-rag.sh query "question"  top 3 chunks
set -euo pipefail

MODEL="${FREELLAMA_EMBED_MODEL:-nomic-embed-text:latest}"
ENDPOINT="${FREELLAMA_ENDPOINT:-http://127.0.0.1:11435}"
STORE="${FREELLAMA_RAG_STORE:-.rag}"
mkdir -p "$STORE"

embed() {  # embed a file of one-item-per-line, print the JSON response
  freellama task --task embedding --model "$MODEL" --endpoint "$ENDPOINT" \
    --input-file "$1" "unused" 2>/dev/null
}

case "${1:-}" in
  index)
    dir="${2:?usage: local-rag.sh index <dir>}"
    # One chunk per line: strip newlines so a chunk survives the line-oriented input format.
    : > "$STORE/corpus.txt"; : > "$STORE/paths.txt"
    while IFS= read -r f; do
      awk -v f="$f" 'BEGIN{RS="\n\n"} {gsub(/\n/," "); if (length($0)>80) {print; print f > "/dev/stderr"}}' \
        "$f" 2>>"$STORE/paths.txt" >> "$STORE/corpus.txt"
    done < <(find "$dir" -type f \( -name '*.rs' -o -name '*.ts' -o -name '*.md' \))
    n=$(wc -l < "$STORE/corpus.txt" | tr -d ' ')
    embed "$STORE/corpus.txt" | jq '.response.embeddings' > "$STORE/vectors.json"
    echo "indexed $n chunks -> $STORE/vectors.json ($(wc -c < "$STORE/vectors.json") bytes)"
    echo "NOTE: this index is a snapshot. Re-run after the corpus changes; nothing detects staleness for you."
    ;;
  query)
    q="${2:?usage: local-rag.sh query <question>}"
    printf '%s\n' "$q" > "$STORE/q.txt"
    embed "$STORE/q.txt" | jq '.response.embeddings[0]' > "$STORE/qvec.json"
    # cosine similarity, top 3 — the only maths RAG actually needs
    jq -r --slurpfile q "$STORE/qvec.json" '
      def dot($a;$b): reduce range(0;$a|length) as $i (0; . + ($a[$i]*$b[$i]));
      def norm($a): dot($a;$a)|sqrt;
      [ to_entries[] | {i:.key, s:(dot(.value;$q[0]) / ((norm(.value)*norm($q[0]))+1e-9))} ]
      | sort_by(-.s)[:3][] | "\(.s|.*1000|round/1000)\t\(.i)"' "$STORE/vectors.json" \
    | while IFS=$'\t' read -r score idx; do
        printf '%s  %s\n     %s\n' "$score" "$(sed -n "$((idx+1))p" "$STORE/paths.txt")" \
          "$(sed -n "$((idx+1))p" "$STORE/corpus.txt" | cut -c1-100)"
      done
    ;;
  *) sed -n '2,16p' "$0"; exit 1 ;;
esac
