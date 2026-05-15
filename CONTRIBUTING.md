# Contributing to Tomcat-RS Compatibility Runtime

Thanks for your interest in the project. It is an early MVP, so there is plenty
of room to help — and contributions are very welcome.

## Building and testing

```sh
cargo build              # build the workspace
cargo build --release    # optimized build
cargo test               # run the test suite
```

The JVM bridge is gated behind the optional `jvm` feature on
`tomcatrs-servlet-bridge` and requires a JDK (Java 17+). Most of the project
builds and tests fine without it:

```sh
cargo build -p tomcatrs-servlet-bridge --features jvm
```

## Code style

Before sending a change, run:

```sh
cargo fmt --all          # format
cargo clippy --all-targets --all-features   # lint
```

CI expects a clean `cargo fmt` and (eventually) no `clippy` warnings. Keep
functions small, prefer explicit error types from `tomcatrs-core`, and
document public items.

## Before submitting a PR

The CI workflows under `.github/workflows/` will run the full suite, but
running the same commands locally first keeps the feedback loop short:

```sh
cargo fmt --all --check                       # formatting gate
cargo test --workspace                        # 864-test default suite
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo build -p tomcatrs-coyote --no-default-features    # tls feature off
```

If you have a JDK 21 installed and are touching the bridge:

```sh
cargo test -p tomcatrs-servlet-bridge --features jvm
```

Note that CI also runs the supply-chain suite (`cargo audit`,
`cargo deny`, OSV-Scanner) on every PR and on a daily cron — see the
"Continuous security checks" section in [`SECURITY.md`](SECURITY.md).
You do not need to install those tools to open a PR, but if you want to
mirror the security job locally:

```sh
cargo install cargo-audit cargo-deny --locked
cargo audit
cargo deny --workspace check
```

Clippy is currently configured to **report** rather than **block** in CI
while we burn down a small backlog of existing warnings. Please do not
add new warnings, and if you can knock one or two off the existing list
while you are in the area, that is very welcome.

## Project layout

The workspace follows a **crate-per-subsystem** layout. Each crate under
`crates/` owns one concern (connectors, container, config, sessions, security,
and so on — see the README's workspace layout table). When adding
functionality, put it in the crate that owns that concern rather than reaching
across crate boundaries. Cross-cutting types belong in `tomcatrs-core`.

## Commit conventions

- Write commits in the imperative mood ("Add HTTP/2 frame decoder", not
  "Added" or "Adds").
- Keep commits focused; one logical change per commit.
- Reference issues in the body where relevant.
- A short type prefix is encouraged: `feat:`, `fix:`, `docs:`, `test:`,
  `refactor:`, `chore:`.

## Where help is most wanted

Two areas need the most attention right now:

- **The JVM bridge** (`tomcatrs-servlet-bridge`) — JNI plumbing, classloader
  isolation, and end-to-end servlet invocation.
- **HTTP/2** (`tomcatrs-coyote`) — moving the HTTP/2 connector from scaffold to
  a working implementation.

Other welcome areas: AJP, TLS termination, the Jasper bridge, clustering, the
Manager API, and growing the compatibility test corpus (see `tests/README.md`).

## License of contributions

By contributing, you agree that your contributions will be licensed under the
Apache License, Version 2.0, consistent with the rest of the project.
