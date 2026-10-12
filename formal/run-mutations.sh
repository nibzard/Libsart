#!/usr/bin/env bash
# Formal mutation checks (SPEC 15.6, Appendix A.9).
#
# Applies each documented model mutation from formal/mutations/ to a copy of
# formal/MarginReview.tla, parses the mutant with SANY, and model checks it
# with TLC using the configuration named in formal/mutations/cases.tsv. A
# defect case passes only when TLC reports the designated detector for that
# defect and the preserved counterexample trace ends with the expected
# action. A control case must pass. SANY failures, timeouts, unrelated
# invariant failures, and missing traces are broken tests, not evidence.
#
# Usage: formal/run-mutations.sh [log-directory]
# The log directory defaults to formal/evidence/mutations. Each case writes
# tlc-<id>.log, sany-<id>.log, and <id>.counterexample.json there, plus
# environment.txt and summary.tsv for the whole run.
#
# The jar is the same pinned official artifact as formal/run-profiles.sh:
# downloaded when absent and verified against the tracked checksum.

set -euo pipefail

version=1.8.0
url="https://github.com/tlaplus/tlaplus/releases/download/v${version}/tla2tools.jar"

here=$(cd "$(dirname "$0")" && pwd)
tools="${here}/tools"
jar="${tools}/tla2tools-${version}.jar"
cases="${here}/mutations/cases.tsv"
out=${1:-${here}/evidence/mutations}

# The script changes directory before writing its logs, so a relative log
# directory is anchored to the invocation directory while it still applies.
case $out in
/*) ;;
*) out="$(pwd)/$out" ;;
esac

for tool in patch python3; do
  command -v "${tool}" >/dev/null 2>&1 || {
    echo "run-mutations.sh needs ${tool} in PATH" >&2
    exit 2
  }
done
[ -f "${cases}" ] || { echo "missing ${cases}" >&2; exit 2; }

mkdir -p "${tools}" "${out}"

if [ ! -f "${jar}" ]; then
  echo "downloading ${url}" >&2
  curl -fsSL --retry 3 -o "${jar}" "${url}"
fi

(cd "${tools}" && sha256sum -c "tla2tools-${version}.jar.sha256") >&2

cd "${here}"

work=$(mktemp -d)
trap 'rm -rf "${work}"' EXIT

# The last [stateNumber, state] pair of the dumped JSON trace is the state
# that violates the property; the action info before it names the action
# that produced it. Prints that action name, or "malformed" when the file
# is absent or has fewer than two states.
trace_last_action() {
  python3 - "$1" <<'PYTHON'
import json, sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        parts = json.load(handle)["counterexample"]["action"]
except (OSError, KeyError, TypeError, ValueError):
    print("malformed")
    raise SystemExit(0)

last = None
count = 0
for part in parts:
    for entry in part:
        if isinstance(entry, dict):
            last = entry.get("name")
        elif isinstance(entry, list):
            count += 1
print(last if count >= 2 and last else "malformed")
PYTHON
}

summary="${out}/summary.tsv"
printf 'case\tpatches\tconfig\texpected\texit\tresult\ttrace_action\tgenerated\tdistinct\tdepth\twall_s\tmutant_sha256\n' >"${summary}"

{
  echo "run started (UTC): $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "tools jar: ${url}"
  sha256sum "tla2tools-${version}.jar" 2>/dev/null || \
    sha256sum "tools/tla2tools-${version}.jar"
  java -version 2>&1
  echo "machine: $(uname -srm), $(nproc) CPUs," \
    "$(lscpu 2>/dev/null | sed -n 's/^Model name:[[:space:]]*//p' | head -1)"
  echo "base source, configs, and mutation patches sha256:"
  sha256sum MarginReview.tla MarginReview*.cfg mutations/*.patch
} >"${out}/environment.txt"

failures=0
while IFS=$'\t' read -r id patches config expect action; do
  case "${id}" in ''|'#'*) continue ;; esac
  [ -n "${patches}" ] && [ -n "${config}" ] && [ -n "${expect}" ] && \
    [ -n "${action}" ] || {
    echo "${id}: malformed cases.tsv row" >&2
    failures=$((failures + 1))
    continue
  }

  dir="${work}/${id}"
  mkdir -p "${dir}"
  cp MarginReview.tla "${config}.cfg" "${dir}/"
  want_property=${expect#violated:}

  if [ "${patches}" != "-" ]; then
    for patch_file in ${patches}; do
      if [ ! -f "mutations/${patch_file}" ]; then
        echo "${id}: missing mutations/${patch_file}" >&2
        failures=$((failures + 1))
        continue 2
      fi
      # A rejected patch means the canonical model and the documented
      # mutation drifted apart; that is a broken test, not a finding.
      patch -d "${dir}" -p1 --batch --fuzz=0 --silent \
        <"mutations/${patch_file}" || {
        echo "${id}: ${patch_file} does not apply to MarginReview.tla" >&2
        failures=$((failures + 1))
        continue 2
      }
    done
  fi

  # SANY and TLC run inside the case directory so that they parse the
  # mutant and write the counterexample trace there.
  (cd "${dir}" && java -cp "${jar}" tla2sany.SANY MarginReview.tla) \
    >"${out}/sany-${id}.log" 2>&1 || {
    echo "${id}: SANY rejected the mutant (see sany-${id}.log)" >&2
    failures=$((failures + 1))
    continue
  }

  start=$(date +%s)
  set +e
  (cd "${dir}" && java -cp "${jar}" tlc2.TLC -workers 1 -cleanup \
    -dumpTrace json "${id}.counterexample.json" \
    -config "${config}.cfg" MarginReview.tla) >"${dir}/tlc.log" 2>&1
  status=$?
  set -e
  wall=$(( $(date +%s) - start ))

  for trace in "${dir}/${id}.counterexample.json"; do
    [ -f "${trace}" ] && mv "${trace}" "${out}/"
  done
  mv "${dir}/tlc.log" "${out}/tlc-${id}.log"

  stats=$(grep -Eo '[0-9,]+ states generated, [0-9,]+ distinct states found' \
    "${out}/tlc-${id}.log" | tail -1)
  generated=$(echo "${stats}" | sed -n 's/^\([0-9,]*\) states generated.*/\1/p' | tr -d ',')
  distinct=$(echo "${stats}" | sed -n 's/.* \([0-9,]*\) distinct states found/\1/p' | tr -d ',')
  depth=$(sed -n 's/^The depth of the complete state graph search is \([0-9]*\)\.$/\1/p' \
    "${out}/tlc-${id}.log" | tail -1)
  mutant_sha=$(sha256sum "${dir}/MarginReview.tla" | cut -d' ' -f1)

  result=unknown
  trace_result=skipped
  if grep -q 'No error has been found' "${out}/tlc-${id}.log"; then
    result=pass
  elif grep -q "Invariant ${want_property} is violated" \
      "${out}/tlc-${id}.log"; then
    result="violated:${want_property}"
  elif grep -q "Action property ${want_property} is violated" \
      "${out}/tlc-${id}.log"; then
    result="violated:${want_property}"
  elif grep -qE 'Invariant [A-Za-z]+ is violated|Action property [A-Za-z]+ is violated|Temporal properties were violated' \
      "${out}/tlc-${id}.log"; then
    result="violated-other:""$(grep -m1 -E 'Invariant [A-Za-z]+ is violated|Action property [A-Za-z]+ is violated|Temporal properties were violated' "${out}/tlc-${id}.log" | tr '\t' ' ')"
  elif grep -qE 'Error:|Parsing error' "${out}/tlc-${id}.log"; then
    result=error
  fi

  if [ "${expect}" = pass ]; then
    [ "${result}" = pass ] || failures=$((failures + 1))
  else
    if [ "${result}" != "violated:${want_property}" ]; then
      failures=$((failures + 1))
    elif [ "${action}" != "-" ]; then
      trace_result=$(trace_last_action "${out}/${id}.counterexample.json")
      [ "${trace_result}" = "${action}" ] || {
        trace_result="unexpected:${trace_result}"
        failures=$((failures + 1))
      }
    fi
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${id}" "${patches}" "${config}" "${expect}" "${status}" "${result}" \
    "${trace_result}" "${generated:-}" "${distinct:-}" "${depth:-}" \
    "${wall}" "${mutant_sha}" >>"${summary}"

  echo "TLC ${id}: exit ${status}, ${result:-unknown}, trace ${trace_result}"
done <"${cases}"

echo
cat "${summary}"
if [ "${failures}" -ne 0 ]; then
  echo "${failures} mutation case(s) deviated from the expected outcome" >&2
  exit 1
fi
