# tomcatrs-bench

Criterion-driven micro-benchmarks for the hot paths of the Tomcat-RS runtime.
This crate is the **v1.0.0 performance baseline**: every future release should
re-run these benches and compare against the baseline saved here, so we can
catch regressions before they ship.

## What it covers

| Bench file          | Subject                                                                 |
| ------------------- | ----------------------------------------------------------------------- |
| `http1_parse.rs`    | Parsing a typical `GET /` HTTP/1.1 request off an in-memory pipe.       |
| `hpack.rs`          | HPACK encode, decode, and round-trip of a typical HTTP/2 request set.   |
| `uri_normalize.rs`  | `normalize_target` over a corpus of representative request URIs.        |
| `mapper.rs`         | `Mapper::map` against a 100 hosts × 50 contexts × 20 wrappers tree.     |
| `cookie_parse.rs`   | `parse_cookie_header` on a realistic mixed-cookie header.               |

Each bench sets its workload up **outside** the inner measurement loop and
wraps the hot inputs in `criterion::black_box` so the optimizer cannot constant-
fold the body away.

## Running the benches

```sh
# Run every bench in this crate.
cargo bench -p tomcatrs-bench

# Run a single bench file.
cargo bench -p tomcatrs-bench --bench http1_parse

# Filter to a single named bench inside a file.
cargo bench -p tomcatrs-bench --bench hpack -- hpack_decode_typical
```

Criterion writes its data and HTML reports to `target/criterion/`. Open
`target/criterion/report/index.html` in a browser for the full output.

## v1.0.0 baseline workflow

The first run of these benches on a given machine establishes that machine's
v1.0.0 baseline. Criterion stores the baseline under `target/criterion/<bench
name>/base/` automatically — the next run compares against it and prints
"Performance has improved." / "Performance has regressed." annotations.

To name a baseline explicitly (recommended for the v1.0.0 release tag):

```sh
# Snapshot the current numbers under the name `v1.0.0`.
cargo bench -p tomcatrs-bench -- --save-baseline v1.0.0

# Later, after changes, compare against that named baseline.
cargo bench -p tomcatrs-bench -- --baseline v1.0.0
```

A regression of more than a few percent on any of these benches against the
saved `v1.0.0` baseline should block a release until investigated.

## Notes

* The benches are **not** end-to-end load tests — for those see the integration
  tests in the workspace root `tests/` directory. These are micro-benchmarks of
  the per-request hot path.
* Bench numbers are sensitive to CPU, frequency scaling, and background load.
  Run on a quiet machine with a fixed performance governor for comparable
  results.
* This crate sets `publish = false` — it is a workspace-internal tool and is
  never released to crates.io.
