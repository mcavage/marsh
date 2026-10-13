#!/bin/sh
# Start the full benchmark pilot: 16 tasks x 2 repetitions with real agents.
# Bills Anthropic and OpenAI (calibrated estimate ~$15; hard stop at the
# config's budget_usd, $90). About an hour of wall time; run it when no other
# real-VM suites share the Mac, because wall time is a metric.
#
#   MARSH=/path/to/marsh sh evals/benchmark/start-pilot.sh
#
# MARSH must be a build that includes #76 (0.1.4-nightly.20261013173603.g854e312
# or later). MARSH_HOME defaults to a dedicated selected home; OUT to
# target/bench-pilot. Rerunning resumes: finished (task, rep) records are kept.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
MARSH=${MARSH:-marsh}
MARSH_HOME=${MARSH_HOME:-$HOME/.marsh-bench}
OUT=${OUT:-$here/../../target/bench-pilot}
CONFIG=${CONFIG:-$here/config.example.json}
export MARSH_HOME
"$MARSH" --version
mkdir -p "$OUT"
python3 "$here/run.py" --estimate --config "$CONFIG" --tasks "$here/tasks"
status=0
python3 "$here/run.py" --config "$CONFIG" --tasks "$here/tasks" --out "$OUT" --work "$OUT/work" \
  --marsh "$MARSH" --marsh-home "$MARSH_HOME" || status=$?
python3 "$here/report.py" "$OUT" > "$OUT/REPORT.md" || true
"$MARSH" stop
echo "report: $OUT/REPORT.md"
exit "$status"
