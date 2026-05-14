# Tomcat-RS bridge — Java facade scaffold

These sources are the **thin Java side** of the Rust ↔ JVM servlet bridge
(`tomcatrs-servlet-bridge`). They are *scaffold / documentation*: **cargo does
not compile them**. A separate build step (`javac` + `jar`) packages them into
`tomcatrs-bridge.jar`, which is placed on the embedded JVM's **common
classpath** (see `JvmConfig::classpath`).

## What's here

| File | Implements | Role |
|------|------------|------|
| `TomcatRsRequestFacade.java`  | `jakarta.servlet.http.HttpServletRequest`  | Lazy-reading request facade. Holds `nativeRequestId`. |
| `TomcatRsResponseFacade.java` | `jakarta.servlet.http.HttpServletResponse` | Streaming response facade. Holds `nativeResponseId`. |
| `TomcatRsServletContext.java` | `jakarta.servlet.ServletContext`           | Bridges context params / real paths to the Rust webapp registry. |
| `NativeRequest.java`          | —                                          | Package-private holder of the request-side `native` method declarations. |
| `NativeResponse.java`         | —                                          | Package-private holder of the response-side `native` method declarations. |

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

The canonical list of native methods and their JNI signatures lives in Rust in
`src/jni.rs` (`NATIVE_REQUEST_METHODS` / `NATIVE_RESPONSE_METHODS`) and must be
kept in sync with the declarations here.

## Building the jar (outside cargo)

```sh
javac -cp jakarta.servlet-api.jar -d out $(find . -name '*.java')
jar cf tomcatrs-bridge.jar -C out .
```

Then add `tomcatrs-bridge.jar` (and `jakarta.servlet-api.jar`) to
`JvmConfig::classpath`.
