#!/usr/bin/env bash
# Crash-consistency test. Repeatedly SIGKILLs cairnd while it indexes a
# synthetic corpus, then checks that:
#   1. the corpus (user data) is bit-for-bit unchanged, including mtimes;
#   2. the index never needed quarantine (no corruption on restart);
#   3. after a final uninterrupted run, the index equals a clean build.
# Usage: CAIRN_TEST_DIR=/some/scratch tests/chaos.sh [rounds]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd); root=$(dirname "$here")
: "${CAIRN_TEST_DIR:?set CAIRN_TEST_DIR to a scratch directory}"
rounds=${1:-30}
source "$here/env.sh"
bin=$root/target/release
export CAIRN_FOLDER_CTX_DELAY_MS=200 CAIRN_IGNORE_THROTTLE=1
log=$S/chaos.log; : > "$log"

"$here/make_corpus.sh" "$S/home" "${CAIRN_TEST_IMAGES:-}" >/dev/null
python3 -I - "$S/home/Bulk" <<'PY'
import os, sys, random
random.seed(7)
words = "invoice terraform kubernetes budget recipe travel camera robot sensor contract revenue design sketch meeting".split()
d = sys.argv[1]; os.makedirs(d, exist_ok=True)
for i in range(150):
    sub = os.path.join(d, f"group{i % 20}")
    os.makedirs(sub, exist_ok=True)
    with open(os.path.join(sub, f"note_{i}.md"), "w") as f:
        for _ in range(60):
            f.write(" ".join(random.choice(words) for _ in range(12)) + "\n")
PY
snapshot() { (cd "$S/home" && find . -path ./Documents/locked -prune -o -type f -print0 | sort -z | xargs -0 stat -c '%n %s %Y' ; find . -path ./Documents/locked -prune -o -type f -print0 | sort -z | xargs -0 sha256sum) | sha256sum; }
before=$(snapshot)

counts() { "$bin/cairn" status --json | python3 -I -c 'import json,sys; s=json.load(sys.stdin); print(s["files"], s["dirs"], s["chunks"], s["text_vectors"], s["image_vectors"], sum(n for _,n in s["pending"]), s["failed"])'; }
run_to_idle() {
  "$bin/cairnd" >>"$log" 2>&1 & pid=$!
  for _ in $(seq 1 1800); do
    sleep 0.5
    if "$bin/cairn" status 2>/dev/null | grep -q "state:      idle"; then break; fi
  done
  counts; kill -TERM $pid; wait $pid || true
}

# Baseline: clean, uninterrupted build.
rm -rf "$CAIRN_DATA_DIR" "$CAIRN_RUNTIME_DIR"
baseline=$(run_to_idle)
echo "baseline: $baseline"

rm -rf "$CAIRN_DATA_DIR" "$CAIRN_RUNTIME_DIR"
for i in $(seq 1 "$rounds"); do
  "$bin/cairnd" >>"$log" 2>&1 & pid=$!
  sleep "$(python3 -I -c 'import random; print(round(random.uniform(0.3, 4.0), 2))')"
  kill -KILL $pid; wait $pid 2>/dev/null || true
  sleep 0.3
  if pgrep -f "cairnd worker" >/dev/null; then echo "round $i: orphaned worker survived"; exit 1; fi
done
final=$(run_to_idle)
echo "after $rounds kills: $final"

after=$(snapshot)
fail=0
[ "$before" = "$after" ] || { echo "FAIL: corpus changed"; fail=1; }
if grep -q "quarantined\|inconsistent" "$log"; then echo "FAIL: index needed recovery"; grep "quarantined\|inconsistent" "$log" | head; fail=1; fi
[ "$baseline" = "$final" ] || { echo "FAIL: final index differs from clean build"; fail=1; }
[ $fail = 0 ] && echo "PASS: $rounds SIGKILLs, no corruption, identical result, user data untouched"
exit $fail
