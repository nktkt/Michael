#!/usr/bin/env bash
# long-soak.sh — drive a 24-hour soak against a release-build tomcatrs server
# and archive the artefacts CI can't produce.
#
# This wrapper is the operator-facing front door to the soak harness in
# `crates/tomcatrs-soak/`. The Rust binary itself does the heavy lifting; this
# script is the small set of boring-but-load-bearing pieces around it:
#
#   * builds release binaries (so we measure the runtime our users see),
#   * starts the server in the background with a deterministic PID file,
#   * waits until it's actually listening,
#   * runs the soaker with operator-friendly defaults,
#   * captures periodic `ps` snapshots in case the soaker itself goes wrong,
#   * tears the server down at the end (or on Ctrl-C).
#
# Prerequisites:
#   * A clean working tree (`git status` shows nothing surprising).
#   * Free disk space for `target/release/` and the artefacts dir (~200 MB).
#   * `ps` and `pgrep` in PATH (every macOS / Linux developer machine has
#     them). On Linux the soaker reads `/proc/<pid>/status` directly; on
#     macOS it shells out to `ps -M`.
#
# Sample invocation:
#   ./scripts/long-soak.sh
#
# With explicit knobs:
#   PORT=18080 DURATION=86400 CONCURRENCY=64 RPS=500 \
#       OUT=./soak-runs/$(date +%Y%m%d-%H%M) ./scripts/long-soak.sh
#
# What to capture after a run:
#   * `$OUT/soak-report.json` — the soaker's machine-readable record.
#   * `$OUT/soak-stdout.log` — the soaker's green/red summary table.
#   * `$OUT/server.log` — the server's `tracing` output for the whole run.
#   * `$OUT/ps-timeseries.tsv` — the belt-and-braces RSS / thread series.
#   * `$OUT/git-rev.txt` — `git rev-parse HEAD` of the build under test.
#
# Triaging a failed soak:
#   1. Check the green/red lines at the bottom of `soak-stdout.log`. The
#      offending assertion tells you whether it was 2xx-rate, RSS, threads,
#      or p99 — each implies a different first stop.
#   2. Plot `ps-timeseries.tsv` (`gnuplot`, `python -c 'import pandas …'`, or
#      whatever you use). A monotonic RSS climb is the smoking gun for a
#      classic leak; a step-function jump that never falls back implies a
#      cache that grows without bound; a periodic sawtooth is most likely a
#      GC / allocator boundary and probably benign.
#   3. Pull the last 10 minutes of `server.log` and `grep -Ei 'warn|error'`.
#      A repeating warning (mapper, JNI, connection cap) often coincides with
#      the slope of the leak.
#   4. Diff `soak-report.json` against the previous green run: the
#      `warmup_latency` / `final_latency` JSON blobs make
#      `jq '.{warmup_latency,final_latency}'` comparisons painless.
#   5. If RSS grew but threads did not, suspect a heap leak (allocator
#      fragmentation, an unbounded `Vec`, a cache without an eviction policy).
#      If threads grew, suspect a missing `JoinSet`/task-cleanup site or a
#      tokio runtime leak.
#   6. Reproduce locally with `--duration 600` first; the leak will usually
#      be visible inside 10 minutes if it's visible inside 24 hours.

set -euo pipefail

# ----- configuration --------------------------------------------------------
PORT="${PORT:-18080}"
DURATION="${DURATION:-86400}"          # 24 hours
WARMUP="${WARMUP:-300}"                # 5 minutes
CONCURRENCY="${CONCURRENCY:-64}"
RPS="${RPS:-500}"
LEAK_RSS_PCT="${LEAK_RSS_PCT:-15.0}"
LEAK_THREADS="${LEAK_THREADS:-8}"
P99_REGRESSION_PCT="${P99_REGRESSION_PCT:-50.0}"
LOG_LEVEL="${LOG_LEVEL:-info}"
OUT="${OUT:-./soak-runs/$(date +%Y%m%d-%H%M%S)}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

mkdir -p "$OUT"
echo "long-soak.sh: artefacts dir = $OUT"

# Snapshot what we're testing so a 24-hour-old report is still actionable.
( cd "$REPO_ROOT" && git rev-parse HEAD ) > "$OUT/git-rev.txt"
( cd "$REPO_ROOT" && git status --porcelain ) > "$OUT/git-status.txt" || true

# ----- build ---------------------------------------------------------------
echo "long-soak.sh: building release binaries"
( cd "$REPO_ROOT" && cargo build --release -p tomcatrs-cli -p tomcatrs-soak ) \
    2>&1 | tee "$OUT/build.log"

CLI_BIN="$REPO_ROOT/target/release/tomcatrs"
SOAK_BIN="$REPO_ROOT/target/release/tomcatrs-soak"
test -x "$CLI_BIN"  || { echo "missing $CLI_BIN"  >&2; exit 1; }
test -x "$SOAK_BIN" || { echo "missing $SOAK_BIN" >&2; exit 1; }

# ----- temp app-base -------------------------------------------------------
APP_BASE="$(mktemp -d -t long-soak-appbase.XXXXXX)"
cat > "$APP_BASE/index.html" <<'HTML'
<!doctype html>
<html><head><title>long-soak</title></head>
<body>long-soak landing page</body></html>
HTML

# ----- spawn the server ----------------------------------------------------
echo "long-soak.sh: starting server on 127.0.0.1:$PORT"
"$CLI_BIN" run \
    --port "$PORT" \
    --app-base "$APP_BASE" \
    --log-level "$LOG_LEVEL" \
    > "$OUT/server.log" 2>&1 &
SERVER_PID=$!
echo "$SERVER_PID" > "$OUT/server.pid"
echo "long-soak.sh: server pid = $SERVER_PID"

cleanup() {
    echo "long-soak.sh: cleaning up"
    if kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        # Give it 10s for graceful drain, then SIGKILL if needed.
        for _ in $(seq 1 20); do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.5
        done
        kill -9 "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$APP_BASE" || true
}
trap cleanup EXIT INT TERM

# Wait up to 60s for the listener to come up.
for _ in $(seq 1 120); do
    if (echo > "/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
        echo "long-soak.sh: server listening"
        break
    fi
    sleep 0.5
done
if ! (echo > "/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
    echo "long-soak.sh: server failed to start in 60s; see $OUT/server.log" >&2
    exit 1
fi

# ----- belt-and-braces ps time series --------------------------------------
# The Rust soaker tracks RSS / threads itself, but if it crashes mid-run we
# still want a coarse record. This is a tiny shell loop, not a substitute
# for the soaker's own time series.
(
    printf "unix_ms\trss_kib\tthreads\n" > "$OUT/ps-timeseries.tsv"
    while kill -0 "$SERVER_PID" 2>/dev/null; do
        now_ms=$(($(date +%s%N 2>/dev/null || gdate +%s%N 2>/dev/null || python3 -c 'import time;print(int(time.time()*1000_000_000))') / 1000000))
        rss=$(ps -o rss= -p "$SERVER_PID" 2>/dev/null | awk '{print $1}')
        # macOS: `ps -M -p PID` lists header + one row per thread. Linux:
        # `ps -L -p PID` does the equivalent.
        if [[ "$(uname -s)" == "Darwin" ]]; then
            threads=$(($(ps -M -p "$SERVER_PID" 2>/dev/null | wc -l) - 1))
        else
            threads=$(($(ps -L -p "$SERVER_PID" --no-headers 2>/dev/null | wc -l)))
        fi
        printf "%s\t%s\t%s\n" "$now_ms" "${rss:-0}" "${threads:-0}" >> "$OUT/ps-timeseries.tsv"
        sleep 5
    done
) &
PS_LOGGER_PID=$!

# ----- run the soaker ------------------------------------------------------
echo "long-soak.sh: starting soaker for ${DURATION}s (concurrency=$CONCURRENCY rps=$RPS)"
SOAK_EXIT=0
"$SOAK_BIN" \
    --target "http://127.0.0.1:$PORT/" \
    --duration "$DURATION" \
    --warmup "$WARMUP" \
    --concurrency "$CONCURRENCY" \
    --rps "$RPS" \
    --leak-threshold-rss-pct "$LEAK_RSS_PCT" \
    --leak-threshold-threads "$LEAK_THREADS" \
    --p99-regression-pct "$P99_REGRESSION_PCT" \
    --target-pid "$SERVER_PID" \
    --report "$OUT/soak-report.json" \
    --log-level "$LOG_LEVEL" \
    2>&1 | tee "$OUT/soak-stdout.log" || SOAK_EXIT=$?

# Stop the ps-timeseries logger.
kill "$PS_LOGGER_PID" 2>/dev/null || true

echo "long-soak.sh: soaker exit = $SOAK_EXIT"
echo "long-soak.sh: artefacts in $OUT"

exit "$SOAK_EXIT"
