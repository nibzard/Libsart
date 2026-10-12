#!/bin/sh
# The C3 stress exercise of SPEC 17.5 and task T054: against one full-scale
# C3 corpus, every ordinary command must stay inside its explicit limits —
# bounded output, no panic, explicit truncation and omission data, and no
# approval of content the page did not carry. C3 carries no latency budget
# (SPEC 17.5); this exercise records behavior, not speed.
#
# Usage: bench/stress-exercise.sh CORPUS_ROOT EXECUTABLE OUTPUT_DIR
#
# CORPUS_ROOT holds repo/ and manifest.json of one generated full-scale C3
# corpus; OUTPUT_DIR collects the raw evidence this run produces.
set -u

if [ "$#" -ne 3 ]; then
    echo "usage: $0 CORPUS_ROOT EXECUTABLE OUTPUT_DIR" >&2
    exit 2
fi

corpus=$1
executable=$2
output=$3
repo="$corpus/repo"
home="$corpus/gen-home"

mkdir -p "$output"
: > "$output/index.txt"

# One recorded observation: the command's wall time, exit, bounded stdout
# size, and the first lines of any stderr.
observe() {
    name=$1
    shift
    started=$(date +%s%N)
    HOME="$home" timeout 600 "$@" > "$output/$name.out" 2> "$output/$name.err"
    code=$?
    ended=$(date +%s%N)
    ms=$(( (ended - started) / 1000000 ))
    bytes=$(wc -c < "$output/$name.out")
    printf '%s exit=%d ms=%d stdout_bytes=%s\n' "$name" "$code" "$ms" "$bytes" >> "$output/index.txt"
}

cd "$repo" || exit 2

# The selected document and the pathological documents, derived from the
# manifest the same way bench/corpus/gen/plan.rs places them: the selected
# document first, then the single-paragraph document, then the
# repeated-line documents.
selected=$(python3 - "$corpus/manifest.json" <<'PY'
import json, sys

manifest = json.load(open(sys.argv[1]))
print(manifest["layout"]["selected"]["path"])
PY
)

selected=$(python3 - "$corpus/manifest.json" <<'PY'
import json, sys

manifest = json.load(open(sys.argv[1]))
print(manifest["layout"]["selected"]["path"])
PY
)

head=$(python3 - "$corpus/manifest.json" <<'PY'
import json, sys

manifest = json.load(open(sys.argv[1]))
print(manifest["graph"]["head"])
PY
)

paths=$(python3 - "$corpus/manifest.json" <<'PY'
import json, sys

manifest = json.load(open(sys.argv[1]))
parameters = manifest["parameters"]
pathological = parameters["pathological"]


def document_path(index):
    if index == 0:
        return "writing/selected.md"
    return "writing/d%02d/doc-%05d.md" % (index % 8, index)


next_index = 1
if pathological["mass_paragraph_bytes"] > 0:
    print(document_path(next_index))
    next_index += 1
for _ in range(pathological["repeated_line_documents"]):
    print(document_path(next_index))
    next_index += 1
PY
)

# Read-only commands over the full stress shape.
observe status "$executable" status --json
observe log "$executable" log --limit 50 --json
observe diff-selected "$executable" diff --path "$selected" --json
observe diff-whole "$executable" diff --json
observe show-selected "$executable" show --path "$selected" --revision "$head" --json

# The pathological documents: the included 10 MiB single-paragraph
# document and the repeated-line cases, when the corpus records them.
for path in $paths; do
    if [ "$path" = "$selected" ]; then
        continue
    fi
    safe=$(printf '%s' "$path" | tr / _)
    observe "show-$safe" "$executable" show --path "$path" --revision "$head" --json
    observe "diff-$safe" "$executable" diff --path "$path" --json
done

# A bounded review over the stress scope must page, never flood: the first
# page of the whole-scope diff with a tight limit.
observe diff-limited "$executable" diff --limit 10 --json

# The repository state must be untouched by every read above.
observe git-check git status --porcelain

cat "$output/index.txt"
