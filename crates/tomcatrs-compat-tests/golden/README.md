# Goldens

Each `*.golden` file in this directory pins the observable response of one
[`CompatScenario`](../src/lib.rs).

The format (see `src/goldens.rs` for the parser) is line-oriented:

```text
status: 200
header.content-type: text/html; charset=utf-8
present.server:
body.contains: Tomcat-RS
body.len_ge: 100
```

* `status: <code>` — required status code.
* `header.<name>: <value>` — header must be present *and* equal `<value>`.
* `present.<name>:` — header must be present, any value.
* `body.contains: <substring>` — body must contain the substring (UTF-8).
* `body.len_ge: <bytes>` — body must be at least this many bytes long.

Lines starting with `#` and blank lines are ignored. Header names are
case-insensitive.

## Why goldens instead of byte-exact response captures?

A byte-exact capture would pin the `Date:` header and the exact HTML body the
adapter happens to render, both of which churn for reasons unrelated to
behavioural compatibility. The substring / presence checks here let two
implementations (Tomcat-RS and a future external Apache Tomcat backend) match
the same golden as long as they agree on what actually matters: status, the
small handful of semantic headers, and the body's gist.
