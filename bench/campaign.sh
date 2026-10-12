#!/bin/sh
# The T054 gated measurement campaign: every primary metric of SPEC 17.4
# against C1 and C2 at full scale, under the SPEC 17.7 sampling policy,
# on the registered runner. Each invocation writes one tracked record in
# bench/results/ and one raw sample stream in bench/results/raw/.
#
# Usage: bench/campaign.sh CORPORA_ROOT EXECUTABLE LOGDIR
#
# CORPORA_ROOT holds corpora/c1 and corpora/c2 at full scale; LOGDIR
# collects one output file per invocation. The campaign refuses to start
# unless the one-minute load average sits under LOAD_LIMIT, because a
# loaded runner measures the load, not the code (SPEC 17.7.4).
set -u

LOAD_LIMIT=${LOAD_LIMIT:-3.0}

if [ "$#" -ne 3 ]; then
    echo "usage: $0 CORPORA_ROOT EXECUTABLE LOGDIR" >&2
    exit 2
fi

corpora=$1
executable=$2
logs=$3

load=$(uptime | sed 's/.*average: //' | cut -d, -f1 | tr -d ' ')
if [ "$(awk -v current="$load" -v ceiling="$LOAD_LIMIT" 'BEGIN { print (current < ceiling) ? 1 : 0 }')" != 1 ]; then
    echo "refusing to measure: load average $load is at or over $LOAD_LIMIT" >&2
    exit 3
fi

mkdir -p "$logs"

# Every primary metric, cheapest first, so an early failure surfaces fast.
for corpus in c1 c2; do
    for metric in P-HELP P-HISTORY P-STATUS P-DIFF P-OPEN P-REFRESH \
        P-CHECKPOINT P-APPLY P-KEY P-RSS P-TREE-MEM P-CPU; do
        log="$logs/$metric.$corpus.log"
        echo "=== $metric on $corpus ($(date -u +%H:%M:%S), load $(cut -d' ' -f1 /proc/loadavg)) ===" | tee -a "$logs/progress.txt"
        if cargo run --quiet --example benchrun -- run \
            --metric "$metric" --corpus "$corpus" \
            --corpus-dir "$corpora/$corpus" \
            --executable "$executable" >"$log" 2>&1; then
            tail -3 "$log" | head -2
        else
            echo "FAILED: see $log" | tee -a "$logs/progress.txt"
            tail -5 "$log"
        fi
    done
done

echo "=== verdicts ==="
for record in bench/results/P-*.json; do
    python3 - "$record" <<'PY'
import json, sys
record = json.load(open(sys.argv[1]))
name = sys.argv[1].split("/")[-1]
number = record.get("p95_ms") or (record.get("details") or {}).get("statistics", {}).get("max_ms")
print("%-22s %-13s %s" % (name, record["status"], number if number is not None else ""))
PY
done
