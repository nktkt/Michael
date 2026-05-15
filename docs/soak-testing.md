# Soak / load-stability testing

This doc describes the soak harness in `crates/tomcatrs-soak/` and the operator
wrapper in `scripts/long-soak.sh`. Together they answer one question: *is the
runtime stable under sustained traffic?* If a 24-hour run holds steady RSS,
steady thread count, steady tail latency, and ≥ 99.5 % 2xx, we treat the
build as soak-clean.

## What the harness measures

| Metric | Source | What a regression looks like |
|---|---|---|
| 2xx rate | the soaker's own request log | hot 5xx burst; connection refused under load; mapper failure |
| p50 / p90 / p99 / p99.9 latency | hand-rolled fixed-bucket histogram in `crates/tomcatrs-soak/src/histogram.rs` | tail climbs over time; mean stays flat ⇒ a slow code path is being hit more often |
| RSS (resident set size) | `/proc/<pid>/status:VmRSS` on Linux; `ps -o rss=` on macOS | monotonic climb across hours; *not* a sawtooth (allocators) |
| live thread count | `/proc/<pid>/status:Threads` on Linux; `ps -M -p PID` line count on macOS | step-function climb that never falls back |

Latency, RSS, and threads are sampled into time series; the soaker compares a
**baseline** (median over the warmup window) to a **final** (median over the
last 60 s) and asserts that growth fits inside operator-chosen thresholds.

## Why hand-rolled buckets?

The default histogram is a 7-decade × 9-sub-bucket table in microseconds
(1 µs..10 s), plus an overflow row and an exact max gauge. That gives 11 %
quantile-step resolution everywhere in the dynamic range — coarse enough to
be cheap, fine enough to spot a doubling of p99. `hdrhistogram` is fine if you
ever need a tighter resolution; the bucket table is `pub(crate)`, so swapping
the implementation is a one-file change.

## CLI shape

The full surface is in `crates/tomcatrs-soak/src/main.rs::Args`. The flags an
operator will reach for in practice:

| Flag | Default | Notes |
|---|---|---|
| `--target` | `http://127.0.0.1:8080/` | Plain `http://host:port/path?query`. No HTTPS / HTTP/2 yet. |
| `--duration` | `300` (5 min) | Total wall clock, including warmup. |
| `--warmup` | `30` | Measurements in this window form the baseline. |
| `--concurrency` | `32` | Max in-flight requests. |
| `--rps` | `200` (`0` = closed-loop) | Open-loop target rate via a shared token bucket. |
| `--leak-threshold-rss-pct` | `15.0` | % growth allowed from baseline → final. |
| `--leak-threshold-threads` | `8` | Absolute thread-count growth allowed. |
| `--p99-regression-pct` | `50.0` | % p99 growth allowed from baseline → final. |
| `--target-pid` | (required for leak checks) | Soaker polls `/proc/<pid>/status` / `ps`. |
| `--report` | `soak-report.json` | Machine-readable record. |

Each assertion prints a green `[OK]` / red `[FAIL]` line; the process exits
non-zero on any failure so CI can wire it up directly.

## Three soak shapes

### 1. CI smoke (≤ 60 s)

The integration test `crates/tomcatrs-soak/tests/short_soak.rs` runs a 30 s
soak against a freshly-spawned `tomcatrs` binary on a random port. It asserts
the exit code is 0, ≥ 99.5 % 2xx, and ≤ 15 % RSS growth. This catches gross
regressions (a leak that doubles RSS in seconds, a panic on 1 % of requests).

```
$ cargo test -p tomcatrs-soak --test short_soak
```

### 2. Developer pre-flight (5–10 min)

Build everything release, then run the soaker directly. Visible leaks in a
24 h run are *almost always* visible inside 10 minutes too — they're just
smaller. This is the cheapest pre-merge check that a refactor didn't grow a
new leak:

```
$ cargo build --release -p tomcatrs-cli -p tomcatrs-soak
$ ./target/release/tomcatrs run --port 18080 --app-base ./webapps &
$ ./target/release/tomcatrs-soak \
    --target http://127.0.0.1:18080/ \
    --duration 600 --warmup 60 --concurrency 32 --rps 200 \
    --target-pid $!
```

### 3. 24-hour validation (`scripts/long-soak.sh`)

The operator wrapper that we run for release candidates. It:

* builds release binaries,
* starts the server with an isolated app-base and PID-tracked,
* runs the soaker for `${DURATION:-86400}` seconds,
* spawns a belt-and-braces `ps` time series in shell (in case the Rust soaker
  itself somehow exits early),
* tears down on success or interrupt,
* archives every artefact under `./soak-runs/<timestamp>/`.

```
$ ./scripts/long-soak.sh
$ ./scripts/long-soak.sh   # with overrides
DURATION=86400 CONCURRENCY=64 RPS=500 OUT=./soak-runs/rc-001 ./scripts/long-soak.sh
```

## Expected baseline numbers on a developer machine

These come from running the 30 s `short_soak` integration test against the
v1.0.0 release on the project author's M-series MacBook (8 workers, 100 rps,
serving a 50-byte `index.html` from `--app-base`).

| Metric | Value |
|---|---|
| 2xx rate | 100.000 % |
| post-warmup p50 | ~0.6 ms |
| post-warmup p90 | ~0.8 ms |
| post-warmup p99 | ~2.0 ms |
| post-warmup p99.9 | ~3.0 ms |
| baseline RSS | ~7–9 MiB |
| final RSS | ~7–9 MiB |
| live threads | ~15–19 |

These numbers describe a server hit at 100 rps. A real 24 h validation should
use the long-soak default of 500 rps; expect threads to settle a few above
this figure (one per inbound connection that the accept loop holds open) and
RSS to climb during warmup but plateau by the end of the first minute.

## Baseline results

The first checked-in soak run lives at
[`soak-runs/v1.0.1-baseline/`](../soak-runs/v1.0.1-baseline/). It is the
reference point future regression hunts should diff against. The exact
invocation, after `cargo build --release -p tomcatrs-cli -p tomcatrs-soak`
and starting the server on a free port `$PORT` with a temp `app-base` and
PID `$SERVER_PID`:

```sh
./target/release/tomcatrs-soak \
    --target "http://127.0.0.1:$PORT/" \
    --duration 600 --warmup 60 --concurrency 32 --rps 200 \
    --target-pid "$SERVER_PID" \
    --report soak-runs/v1.0.1-baseline/soak-report.json
```

10 minutes of sustained 200 rps against a release build of `tomcatrs` v1.0.0
serving the static `webapps/ROOT/index.html` on an Apple-silicon laptop:

| Dimension | Baseline (warmup median) | Final (last-60 s median) | Delta |
|---|---|---|---|
| Duration | 600 s | — | — |
| Concurrency | 32 | — | — |
| Open-loop target | 200 rps | actual ~200 rps (108 001 requests over 540 s of post-warmup window) | — |
| 2xx rate | — | 100.000 % (108 001 / 108 001) | — |
| Latency p50 | 0.30 ms | 0.20 ms | −33 % |
| Latency p99 | 2.00 ms | 1.00 ms | −50 % |
| Latency p99.9 | 20.00 ms | 10.00 ms | −50 % |
| RSS | 4 032 KiB | 2 592 KiB | −35.71 % |
| Live thread count | 24 | 24 | +0 |

All four assertions held:

```
[OK]   2xx rate >= 99.500%   (actual 100.000%)
[OK]   p99 latency <= baseline x 1.50   (baseline 2.00ms, final 1.00ms, allowed 3.00ms)
[OK]   RSS growth <= 15.00%   (baseline 4032 KiB, final 2592 KiB, allowed 4637 KiB)
[OK]   thread growth <= +8   (baseline 24, final 24, allowed <= 32)
```

The `server.log` is empty: at `--log-level warn` the runtime emitted no
warnings or errors for the entire 10 minutes — i.e., no `recv` reset, no
mapper miss, no connection cap rejection.

This is the **v1.0.1 baseline**. Future PRs that touch the hot connector or
mapper paths should re-run the same command and diff
`final_process.rss_kib`, `final_latency.p99_us`, `final_latency.p99_9_us`,
and `final_process.threads` against this report. Any regression that crosses
the 15 % RSS / +8 threads / 50 % p99 thresholds will fail the assertion gate
and the soak's exit code; tighten or document accordingly.

### What 10 minutes does — and does not — prove

A clean 600 s soak rules out a large class of leaks:

* fast heap leaks (anything that climbs more than 15 % of baseline inside
  10 minutes is visible),
* per-request thread leaks (an accept-loop bug that spawns without joining
  shows up within seconds),
* tail-latency cliffs (a regex that backtracks, a lock-contention deadband,
  or a slow path that's hit on every Nth request).

It does **not** prove the absence of:

* hour-scale leaks (a cache that bounds to 50 MiB and then plateaus would
  look identical at minute 10 and minute 600),
* day-scale clock / monotonic-time bugs (an `Instant` arithmetic overflow
  past 2³² ms ≈ 49.7 days),
* drift in the JVM bridge under realistic servlet traffic (the soak hits
  the static handler; servlet WARs exercise a different code path).

For release-candidate validation, run `scripts/long-soak.sh` (24 h default)
and archive the artefacts under a fresh `soak-runs/<tag>/` directory. The
10-minute baseline is the *cheap* gate; the 24-hour soak is the
*sufficient* one.

## Attributing a regression

Treat each failed assertion as a different category of bug; the runbook is in
`scripts/long-soak.sh`'s header comment, summarised here:

* **2xx rate fell** — almost always a server-side panic in a request handler
  or a connection-cap exhaustion. Start in `server.log` filtered to
  `WARN`/`ERROR`.
* **RSS climbed monotonically** — heap leak. The classic shapes are an
  unbounded cache, a `Vec` that's only appended to, or fragmentation in a
  custom allocator. Diff with the last green report's `final_process.rss_kib`.
* **Threads climbed monotonically** — task or thread leak. Look for missing
  `JoinHandle::abort()` / `JoinSet::shutdown()` on a server-shutdown path,
  or per-request `tokio::spawn` without a join.
* **p99 climbed but RSS held** — a code path is being hit more often *or* a
  data structure is being walked more often. Compare `warmup_latency.p99_us`
  to `final_latency.p99_us` and sweep the recent commits that touched the hot
  path.

## Pinning the report format

The JSON schema is whatever `SoakReport` in `src/main.rs` serializes to. We
deliberately don't promise stability across crate versions: the report is a
diagnostic, not a public API. CI scripts that read it should target the top
two or three fields (`two_xx_rate`, `baseline_process.rss_kib`,
`final_process.rss_kib`, `warmup_latency.p99_us`, `final_latency.p99_us`) and
tolerate added keys.
