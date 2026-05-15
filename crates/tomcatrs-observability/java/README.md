# tomcatrs-observability — Java companion sources

This tree contains the Java side of the **JMX bridge** for `tomcatrs-observability`.

## Layout

- `org/apache/tomcatrs/jmx/TomcatRsMetricsMBean.java` — a `javax.management.DynamicMBean`
  whose single read-only `Value` attribute proxies into Rust via the native
  method `nativeGetMetricValue(String)`. One instance is registered per Rust-side
  metric by `JmxBridge::register_with_jvm` under the object name
  `TomcatRS:type=Metric,name=<metric>`.
- `javax-stubs/javax/management/*.java` — minimal compile-time stand-ins for
  the `javax.management` types referenced by the MBean. They mirror the
  `jakarta-stubs/` approach used by `tomcatrs-servlet-bridge` (with one wrinkle:
  see below).

## Building standalone

`javax.management` is part of the JDK's built-in `java.management` module, so
unlike `jakarta.servlet` (a third-party namespace), our stubs cannot simply be
placed on the classpath — javac rejects "package present in another module"
declarations. The supported way to compile the stubs alongside the JDK's own
copy of the package is `--patch-module`:

```sh
javac -d build/classes \
      --patch-module java.management=javax-stubs \
      $(find javax-stubs org -name '*.java')
```

In production the real `java.management` module on the runtime classpath
replaces these stubs. If you have a JDK available you can also skip the stubs
entirely:

```sh
javac -d build/classes org/apache/tomcatrs/jmx/TomcatRsMetricsMBean.java
```

The stubs exist primarily so reviewers and CI can confirm the Java side
type-checks against an exact, minimal API surface.

## Native method

`TomcatRsMetricsMBean.nativeGetMetricValue(String)` is registered by the
embedding Rust process via `JNIEnv::register_native_methods`, not loaded from
a shared library. The `static` initializer tolerates a missing `.so` to keep
class loading working when Tomcat-RS is the launcher.
