#!/usr/bin/env bash
# Live-behaviour test against a running daemon on the synthetic corpus:
# rename adds context, moves keep identity, exclusions hide and survive a
# rename, include restores, new notes change folder context.
# Usage: CAIRN_TEST_DIR=/scratch [CAIRN_TEST_IMAGES=dir] tests/behaviour.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd); root=$(dirname "$here")
: "${CAIRN_TEST_DIR:?}"
source "$here/env.sh"
bin=$root/target/release
export CAIRN_FOLDER_CTX_DELAY_MS=300 CAIRN_IGNORE_THROTTLE=1
"$here/make_corpus.sh" "$S/home" "${CAIRN_TEST_IMAGES:-}" >/dev/null
rm -rf "$CAIRN_DATA_DIR" "$CAIRN_RUNTIME_DIR"
"$bin/cairnd" >"$S/behaviour.log" 2>&1 & pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
idle() { for _ in $(seq 1 240); do sleep 0.5; "$bin/cairn" status 2>/dev/null | grep -q "state:      idle" && return 0; done; echo "timeout"; return 1; }
q() { "$bin/cairn" search --json --limit 10 "$@" | python3 -I -c 'import json,sys; print("\n".join(h["path"]["path"] for h in json.load(sys.stdin)["hits"]))'; }
fail=0; check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }
idle

# 1. Renaming a context-free folder makes its files findable by the new name.
mv "$S/home/Zed/infra-tools" "$S/home/Zed/aws staging provisioning"; sleep 2; idle
check "rename adds context" '[[ "$(q staging provisioning)" == *"aws staging provisioning/deploy.sh"* ]]'

# 2. A moved file keeps its content index (no re-extraction needed to be found).
mv "$S/home/Zed/aws staging provisioning/utils.py" "$S/home/Documents/utils.py"; sleep 2; idle
check "moved file found by content at new path" '[[ "$(q function that parses config)" == *"Documents/utils.py"* ]]'

# 3. Exclusion hides a folder immediately, and survives a rename.
"$bin/cairn" exclude "$S/home/Zed/acme-portal" >/dev/null
check "excluded folder hidden" '[[ "$(q SOP document client)" != *acme-portal* ]]'
mv "$S/home/Zed/acme-portal" "$S/home/Zed/acme-renamed"; sleep 2; idle
check "exclusion survives rename" '[[ "$(q SOP document client)" != *acme* ]]'

# 4. Marker file excludes too.
touch "$S/home/Documents/Finance/.noindex"; sleep 2; idle
check ".noindex marker hides folder" '[[ "$(q sales report)" != *Finance* ]]'
rm "$S/home/Documents/Finance/.noindex"; "$bin/cairn" rescan >/dev/null; sleep 3; idle
check "removing marker restores" '[[ "$(q sales report)" == *Finance* ]]'

# 5. Writing a note into a folder gives its files new meaning.
mkdir -p "$S/home/Pictures/2024-07"; cp "$S/home/Unfinished/robot-arm/plan.md" "$S/home/Pictures/2024-07/x.md"
printf 'Wedding of Priya and Arjun in Udaipur\n' > "$S/home/Pictures/2024-07/notes.txt"; sleep 2; idle
check "notes give folder context" '[[ "$(q wedding udaipur)" == *"Pictures/2024-07"* ]]'

# 6. Deleting a file removes it.
rm "$S/home/Documents/utils.py"; sleep 2; idle
check "deleted file gone" '[[ "$(q function that parses config)" != *"Documents/utils.py"* ]]'

# 7. Folder named in the query limits results; --in does the same.
if [ -n "${CAIRN_TEST_IMAGES:-}" ]; then
  check "\"from downloads\" limits to Downloads" '[[ "$(q beach photos from downloads)" == *Downloads/IMG_9001.jpg* && "$(q beach photos from downloads)" != *Pictures* ]]'
  check "--in limits to folder" '[[ "$("$bin/cairn" search --json --in "$S/home/Pictures" beach photos | grep -c Downloads)" == 0 ]]'
fi
check "\"in finance\" limits to Finance" '[[ "$(q revenue summary in finance)" == *Finance/sales* && "$(q revenue summary in finance)" != *Pictures* ]]'
check "unknown place stays a search word" '[[ "$(q notes from nowhereland)" != *error* ]]'

[ $fail = 0 ] && echo "PASS: behaviour" || echo "FAIL: behaviour"
exit $fail
