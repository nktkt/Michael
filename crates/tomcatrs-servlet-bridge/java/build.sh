#!/usr/bin/env bash
#
# Developer convenience: compile the Tomcat-RS Java bridge into
# `tomcatrs-bridge.jar` by hand, without going through cargo / build.rs.
#
# This mirrors exactly what `crates/tomcatrs-servlet-bridge/build.rs` does when
# the `jvm` feature is enabled:
#
#   1. compile the `jakarta.servlet.*` compile-time stubs + the
#      `org.apache.tomcatrs.bridge.*` facades with `javac`
#   2. package the resulting classes into `tomcatrs-bridge.jar`
#
# The `jakarta-stubs/` tree is a minimal stand-in for the real
# `jakarta.servlet-api` jar so the bridge compiles standalone. In production the
# real Jakarta Servlet API jar is on the classpath instead — see README.md.
#
# Usage:
#   ./build.sh [OUT_DIR]
#
# OUT_DIR defaults to `./build`. The jar is written to `OUT_DIR/tomcatrs-bridge.jar`
# and the intermediate classes to `OUT_DIR/classes`.

set -euo pipefail

# Directory of this script (the `java/` root), regardless of cwd.
JAVA_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT_DIR="${1:-${JAVA_ROOT}/build}"
CLASSES_DIR="${OUT_DIR}/classes"
JAR_PATH="${OUT_DIR}/tomcatrs-bridge.jar"

# Resolve JDK tools, honouring JAVA_HOME when set.
if [[ -n "${JAVA_HOME:-}" && -x "${JAVA_HOME}/bin/javac" ]]; then
  JAVAC="${JAVA_HOME}/bin/javac"
  JAR="${JAVA_HOME}/bin/jar"
else
  JAVAC="javac"
  JAR="jar"
fi

if ! command -v "${JAVAC}" >/dev/null 2>&1; then
  echo "error: '${JAVAC}' not found on PATH (set JAVA_HOME or install a JDK)" >&2
  exit 1
fi

echo "==> javac: $("${JAVAC}" -version 2>&1)"
echo "==> output: ${OUT_DIR}"

rm -rf "${CLASSES_DIR}"
mkdir -p "${CLASSES_DIR}"

# Collect every source file under the stubs + facades trees into an args file
# (portable: no `mapfile`/arrays, handles an arbitrary number of sources).
SOURCES_LIST="${OUT_DIR}/sources.txt"
mkdir -p "${OUT_DIR}"
find "${JAVA_ROOT}/jakarta-stubs" "${JAVA_ROOT}/org" -name '*.java' > "${SOURCES_LIST}"
SOURCE_COUNT="$(wc -l < "${SOURCES_LIST}" | tr -d ' ')"
if [[ "${SOURCE_COUNT}" -eq 0 ]]; then
  echo "error: no .java sources found under ${JAVA_ROOT}" >&2
  exit 1
fi

echo "==> compiling ${SOURCE_COUNT} source files"
"${JAVAC}" -d "${CLASSES_DIR}" -encoding UTF-8 "@${SOURCES_LIST}"

echo "==> packaging ${JAR_PATH}"
"${JAR}" cf "${JAR_PATH}" -C "${CLASSES_DIR}" .

echo "==> done: ${JAR_PATH}"
