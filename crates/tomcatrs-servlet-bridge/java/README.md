# Tomcat-RS bridge — Java side

These sources are the **thin Java side** of the Rust ↔ JVM servlet bridge
(`tomcatrs-servlet-bridge`). They are **real, compilable Java**: the bridge
facades implement the `jakarta.servlet.*` interfaces, hold an opaque `long`
handle into the Rust side, and delegate every accessor to a `native` method.

They are packaged into **`tomcatrs-bridge.jar`**, which is placed on the
embedded JVM's **common classpath** (see `JvmConfig::classpath`).

## Layout

```
java/
├── README.md                  ← this file
├── build.sh                   ← developer convenience: build the jar by hand
├── jakarta-stubs/             ← compile-time stubs of the Jakarta Servlet API
│   └── jakarta/servlet/...     (NOT shipped to production — see "Stub strategy")
└── org/apache/tomcatrs/bridge/ ← the real bridge classes
    ├── TomcatRsBridge.java         entrypoint: native lifecycle hooks + ServletDispatcher
    ├── TomcatRsRequestFacade.java  HttpServletRequest facade (holds nativeRequestId)
    ├── TomcatRsResponseFacade.java HttpServletResponse facade (holds nativeResponseId)
    ├── TomcatRsServletContext.java ServletContext facade (keyed by contextPath)
    ├── NativeRequest.java          request-side `static native` declarations
    └── NativeResponse.java         response-side `static native` declarations
```

| File | Implements | Role |
|------|------------|------|
| `TomcatRsRequestFacade.java`  | `jakarta.servlet.http.HttpServletRequest`  | Lazy-reading request facade. Holds `nativeRequestId`. |
| `TomcatRsResponseFacade.java` | `jakarta.servlet.http.HttpServletResponse` | Streaming response facade. Holds `nativeResponseId`. |
| `TomcatRsServletContext.java` | `jakarta.servlet.ServletContext`           | Bridges context params / real paths to the Rust webapp registry. |
| `NativeRequest.java`          | —                                          | Package-private holder of the request-side `native` method declarations. |
| `NativeResponse.java`         | —                                          | Package-private holder of the response-side `native` method declarations. |
| `TomcatRsBridge.java`         | —                                          | Entrypoint: `native` lifecycle hooks (`nativeOnStart` / `nativeOnShutdown` / `nativeReportError`) and the `ServletDispatcher` helper. |

## Design

* **Opaque ids only.** A facade holds a single `long` (`nativeRequestId` /
  `nativeResponseId`). That is the only value that crosses JNI; everything else
  is pulled lazily.
* **Lazy materialization.** Each getter (`getHeader`, `getParameter`, …) is a
  thin delegate to a `native` method. A header the servlet never reads never
  crosses the boundary.
* **Streaming bodies.** `getInputStream()` / `getOutputStream()` return streams
  whose `read` / `write` call `nativeReadBody` / `nativeWriteBody` — no
  whole-payload buffer on the Java heap.
* **Natives are *registered*, not *loaded*.** The `static`
  `System.loadLibrary(...)` blocks are deliberately tolerant of failure: the
  embedding Rust process registers the native methods with
  `JNIEnv::register_native_methods` at JVM start-up. There is no standalone
  `.so`/`.dll`.
* **Dispatch.** `TomcatRsBridge.ServletDispatcher` builds the request/response
  facades from a pair of opaque native ids and calls `servlet.service(req, res)`
  (or drives a single `Filter` with a terminal `FilterChain`).

The canonical list of native methods and their JNI signatures lives in Rust in
`src/jni.rs` (`NATIVE_REQUEST_METHODS` / `NATIVE_RESPONSE_METHODS`) and must be
kept in sync with the declarations here.

## Stub strategy — `jakarta-stubs/`

The facades `implement jakarta.servlet.*` interfaces, so **something** must
provide those types on the `javac` classpath. For reproducibility — no network
fetch, no vendored binary — `jakarta-stubs/` contains **minimal source stubs**
of just the Jakarta Servlet API types the facades touch
(`HttpServletRequest`, `HttpServletResponse`, `ServletContext`,
`ServletRequest`, `ServletResponse`, `ServletInputStream`,
`ServletOutputStream`, `Servlet`, `ServletConfig`, `ServletException`,
`Filter`, `FilterConfig`, `FilterChain`, `ReadListener`, `WriteListener`).

Each stub declares **only** the members the bridge actually uses.

> **These stubs are not the real Jakarta Servlet API and are not for
> production.** They exist solely so the bridge facades can be *compiled
> standalone*. In a real deployment the genuine `jakarta.servlet-api` jar is on
> the embedded JVM's classpath (and on `javac`'s classpath if you rebuild the
> jar against it); it fully supersedes these stubs. Because the bridge classes
> only reference the subset of the API the stubs declare, they compile
> unchanged against either.

## Building the jar

### Via cargo (the `jvm` feature)

`crates/tomcatrs-servlet-bridge/build.rs` builds the jar automatically:

```sh
cargo build -p tomcatrs-servlet-bridge --features jvm
```

When `CARGO_FEATURE_JVM` is set, `build.rs` runs `javac` over
`jakarta-stubs/` + `org/`, packages the classes into
`$OUT_DIR/tomcatrs-bridge.jar`, and exports the path to Rust as
`env!("TOMCATRS_BRIDGE_JAR")`. With **default features** `build.rs` is a
complete no-op and no JDK is required. If `javac`/`jar` are missing or fail,
`build.rs` prints a `cargo:warning=` and continues — a missing JDK never fails
the build.

### By hand (developer convenience)

```sh
./build.sh            # writes ./build/tomcatrs-bridge.jar
./build.sh /tmp/out   # or pass an explicit output dir
```

`build.sh` mirrors what `build.rs` does. It honours `JAVA_HOME` if set.

### Raw commands

```sh
javac -d out $(find jakarta-stubs org -name '*.java')
jar cf tomcatrs-bridge.jar -C out .
```

Then add `tomcatrs-bridge.jar` to `JvmConfig::classpath` — alongside the real
`jakarta.servlet-api.jar` in production.
