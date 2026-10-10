#!/usr/bin/env bash
# SPEC 15.8 / Appendix A.8 formal evidence gate.
#
# Runs SANY and the five TLC profiles against formal/MarginReview.tla with
# the pinned official tla2tools jar. The four positive profiles must pass;
# MarginReviewUncooperative must fail on NoUnseenOverwrite only.
#
# Usage: formal/run-profiles.sh [log-directory]
# The log directory defaults to formal/evidence/logs. Each run writes
# sany.log, tlc-<profile>.log, environment.txt, and summary.tsv there.
# Counterexample traces are preserved as <profile>.counterexample.json.
#
# The jar is downloaded from the official GitHub release when absent and its
# sha256 is verified against formal/tools/tla2tools-<version>.jar.sha256,
# whose digest is the one GitHub publishes for the release asset.

set -euo pipefail

version=1.8.0
url="https://github.com/tlaplus/tlaplus/releases/download/v${version}/tla2tools.jar"

here=$(cd "$(dirname "$0")" && pwd)
tools="${here}/tools"
jar="${tools}/tla2tools-${version}.jar"
out=${1:-${here}/evidence/logs}

mkdir -p "${tools}" "${out}"

if [ ! -f "${jar}" ]; then
  echo "downloading ${url}" >&2
  curl -fsSL --retry 3 -o "${jar}" "${url}"
fi

(cd "${tools}" && sha256sum -c "tla2tools-${version}.jar.sha256") >&2

cd "${here}"

{
  echo "run started (UTC): $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "tools jar: ${url}"
  sha256sum "tla2tools-${version}.jar" 2>/dev/null || \
    sha256sum "tools/tla2tools-${version}.jar"
  java -version 2>&1
  echo "machine: $(uname -srm), $(nproc) CPUs," \
    "$(lscpu 2>/dev/null | sed -n 's/^Model name:[[:space:]]*//p' | head -1)"
  echo "source and config sha256:"
  sha256sum MarginReview.tla MarginReview*.cfg
} >"${out}/environment.txt"

java -cp "${jar}" tla2sany.SANY MarginReview.tla >"${out}/sany.log" 2>&1
echo "SANY: exit 0"

# Profile order follows Appendix A.8. The last entry is the negative profile
# whose expected failure is the NoUnseenOverwrite violation.
profiles=(
  MarginReview
  MarginReviewLive
  MarginReviewNightly
  MarginReviewCooperative
  MarginReviewUncooperative
)
expected=(
  pass
  pass
  pass
  pass
  fail-no-unseen-overwrite
)

summary="${out}/summary.tsv"
printf 'profile\texpected\texit\tresult\tgenerated\tdistinct\tdepth\twall_s\n' >"${summary}"

failures=0
for i in "${!profiles[@]}"; do
  profile=${profiles[$i]}
  want=${expected[$i]}
  log="${out}/tlc-${profile}.log"

  start=$(date +%s)
  set +e
  # -dumpTrace preserves any counterexample as JSON; positive runs produce
  # no file. The "tla" dump format needs an IOUtils module that the 1.8.0
  # release jar does not bundle, so only json is used.
  java -cp "${jar}" tlc2.TLC -workers 1 -cleanup \
    -dumpTrace json "${profile}.counterexample.json" \
    -config "${profile}.cfg" MarginReview.tla >"${log}" 2>&1
  status=$?
  set -e
  wall=$(( $(date +%s) - start ))

  for trace in "${profile}.counterexample.json"; do
    [ -f "${trace}" ] && mv "${trace}" "${out}/"
  done

  stats=$(grep -Eo '[0-9,]+ states generated, [0-9,]+ distinct states found' "${log}" | tail -1)
  generated=$(echo "${stats}" | sed -n 's/^\([0-9,]*\) states generated.*/\1/p' | tr -d ',')
  distinct=$(echo "${stats}" | sed -n 's/.* \([0-9,]*\) distinct states found/\1/p' | tr -d ',')
  depth=$(sed -n 's/^The depth of the complete state graph search is \([0-9]*\)\.$/\1/p' "${log}" | tail -1)

  if grep -q 'No error has been found' "${log}"; then
    result=pass
  elif grep -q 'Invariant NoUnseenOverwrite is violated' "${log}"; then
    result=violated-NoUnseenOverwrite
  elif grep -qE 'Invariant [A-Za-z]+ is violated|Temporal properties were violated' "${log}"; then
    result=violated-other:"$(grep -m1 -E 'Invariant [A-Za-z]+ is violated|Temporal properties were violated' "${log}" | tr '\t' ' ')"
  else
    result=unknown
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${profile}" "${want}" "${status}" "${result}" \
    "${generated:-}" "${distinct:-}" "${depth:-}" "${wall}" >>"${summary}"

  echo "TLC ${profile}: exit ${status}, ${result:-unknown}"

  if [ "${want}" = pass ] && [ "${result}" != pass ]; then
    failures=$((failures + 1))
  fi
  if [ "${want}" = fail-no-unseen-overwrite ] && [ "${result}" != violated-NoUnseenOverwrite ]; then
    failures=$((failures + 1))
  fi
done

echo
cat "${summary}"
if [ "${failures}" -ne 0 ]; then
  echo "${failures} profile(s) deviated from the expected outcome" >&2
  exit 1
fi
