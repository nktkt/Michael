#!/usr/bin/env bash
# Fetch + build the real Spring Boot 3.x WAR fixture at
# `tests/fixtures/real-wars/spring-boot-app/`, then expand it into
# `spring-boot-app/exploded/` so Tomcat-RS can deploy it as an exploded
# webapp (the project's primary deployment mode).
#
# Why this script exists:
#   The fixture's `pom.xml` declares Spring Boot 3.3.x. We deliberately do NOT
#   commit any of the resulting jars into git — they total ~25 MB and change
#   on every Spring Boot patch release. Instead this script fetches them on
#   demand via Maven, builds the WAR, and unzips it under `exploded/`. The
#   JVM-bridge integration test (`crates/tomcatrs-servlet-bridge/tests/
#   spring_boot.rs`) invokes this once per test run when `exploded/` is
#   missing or empty.
#
# Usage:
#     bash tests/fixtures/real-wars/build-spring.sh
#
# Behaviour:
#   * Checks for `mvn` on PATH; bails with a clear error and `exit 1` if
#     missing (rather than silently skipping). The integration test
#     interprets that exit code as "no Maven, skip the test".
#   * Self-locating: works from any CWD; resolves the project relative to
#     this script.
#   * Idempotent: re-running is safe. If the WAR has already been built and
#     the exploded tree contains the expected marker class, the script is
#     a near-no-op (it still re-explodes, but never tries to be clever).
#   * Compatible with macOS (BSD coreutils) and Linux (GNU coreutils).
#
# Exit codes:
#   0   build + explode succeeded (or nothing to do)
#   1   `mvn` not on PATH, or `mvn package` failed, or the WAR could not be
#       located / expanded.

set -euo pipefail

# --- Self-locate ------------------------------------------------------------

# Resolve the directory containing this script. Works on macOS (no GNU
# `readlink -f`) and Linux alike.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_dir="${script_dir}/spring-boot-app"
pom="${app_dir}/pom.xml"
target_dir="${app_dir}/target"
exploded_dir="${app_dir}/exploded"

if [[ ! -f "${pom}" ]]; then
    echo "build-spring.sh: pom.xml missing at ${pom}" >&2
    echo "build-spring.sh: (expected the fixture project under ${app_dir})" >&2
    exit 1
fi

# --- Check for Maven --------------------------------------------------------

if ! command -v mvn >/dev/null 2>&1; then
    cat >&2 <<'EOF'
build-spring.sh: `mvn` is not on PATH.

This script fetches and builds a real Spring Boot 3.3.x WAR via Apache Maven.
Install Maven (any 3.6+ release works) and a JDK 17+ — then re-run.

On macOS:    brew install maven
On Debian:   apt-get install maven
On RHEL:     dnf install maven

If you are running this from a CI environment that does not provide Maven,
the JVM-bridge integration test will detect a missing build script result
and skip; see `crates/tomcatrs-servlet-bridge/tests/spring_boot.rs`.
EOF
    exit 1
fi

# --- Build the WAR ----------------------------------------------------------
#
# `-B`               Batch mode (no interactive prompts, terse output).
# `-f <pom>`         Use this fixture's POM regardless of the CWD.
# `-DskipTests`      The fixture has no tests of its own — skip Surefire.
#
# Maven downloads dependencies into the user's local repository
# (`~/.m2/repository`) by default; we deliberately do NOT pin a custom
# repository path so a developer's existing cached jars are reused.

echo "build-spring.sh: running 'mvn -B -f ${pom} -DskipTests package'"
if ! mvn -B -f "${pom}" -DskipTests package; then
    echo "build-spring.sh: 'mvn package' failed; aborting" >&2
    exit 1
fi

# --- Locate the produced WAR ------------------------------------------------
#
# The POM pins `finalName=spring-boot-app` so the deployable WAR is at
# `target/spring-boot-app.war`. However the `spring-boot-maven-plugin`
# `repackage` goal — which the parent POM binds to the `package` phase —
# *replaces* the original WAR with an "executable" archive (BOOT-INF/, a
# loader main class, etc.) and renames the original to `<name>.war.original`.
#
# We want the **deployable** WAR (`WEB-INF/classes/` + `WEB-INF/lib/`),
# never the executable one — Tomcat-RS deploys it into its own container
# rather than running `java -jar`. So:
#   * if `<name>.war.original` exists, use that;
#   * otherwise fall back to the lone `*.war` in the target directory.
#
# This makes the script tolerate both POM shapes: with the repackage goal
# (current pom.xml) and, in a hypothetical future, without it.

war_file=""
original_war="${target_dir}/spring-boot-app.war.original"
if [[ -f "${original_war}" ]]; then
    war_file="${original_war}"
    echo "build-spring.sh: using deployable WAR ${war_file} (skipping repackaged executable)"
else
    while IFS= read -r -d '' f; do
        if [[ -n "${war_file}" ]]; then
            echo "build-spring.sh: more than one WAR under ${target_dir}/:" >&2
            echo "  - ${war_file}" >&2
            echo "  - ${f}" >&2
            echo "build-spring.sh: refusing to guess which to deploy" >&2
            exit 1
        fi
        war_file="${f}"
    done < <(find "${target_dir}" -maxdepth 1 -type f -name '*.war' -print0 2>/dev/null)

    if [[ -z "${war_file}" ]]; then
        echo "build-spring.sh: 'mvn package' completed but no *.war is in ${target_dir}/" >&2
        exit 1
    fi

    echo "build-spring.sh: built ${war_file}"
fi

# --- Explode the WAR --------------------------------------------------------
#
# `Webapp::open` and the JVM-bridge integration test both consume an
# exploded webapp directory (i.e. WEB-INF/, plus optional static assets at
# the root). We unzip the WAR into `exploded/`, replacing the previous
# tree, so each run sees the freshly-built bytes.
#
# `unzip -o` overwrites without prompting. We rm-rf first anyway so a class
# file that has since been *removed* from the WAR doesn't linger in the
# exploded tree.

if ! command -v unzip >/dev/null 2>&1; then
    echo "build-spring.sh: 'unzip' is not on PATH; cannot explode WAR" >&2
    echo "build-spring.sh: install unzip (apt-get install unzip / brew install unzip)" >&2
    exit 1
fi

rm -rf "${exploded_dir}"
mkdir -p "${exploded_dir}"

echo "build-spring.sh: exploding into ${exploded_dir}/"
unzip -q -o "${war_file}" -d "${exploded_dir}"

# --- Sanity-check the exploded layout ---------------------------------------
#
# A deployable Spring Boot WAR must contain:
#   WEB-INF/classes/com/example/sbapp/SbApplication.class  (our entry point)
#   WEB-INF/lib/                                            (provided deps)
#
# If either is missing, the integration test would fail with a less
# informative error later; catch it here.

entrypoint_class="${exploded_dir}/WEB-INF/classes/com/example/sbapp/SbApplication.class"
lib_dir="${exploded_dir}/WEB-INF/lib"

if [[ ! -f "${entrypoint_class}" ]]; then
    echo "build-spring.sh: exploded WAR is missing entry-point class:" >&2
    echo "  ${entrypoint_class}" >&2
    exit 1
fi
if [[ ! -d "${lib_dir}" ]]; then
    echo "build-spring.sh: exploded WAR is missing WEB-INF/lib/: ${lib_dir}" >&2
    exit 1
fi

# Count the jars under WEB-INF/lib for the operator's benefit. A real
# Spring Boot 3.3.x WAR with `tomcat-embed-*` excluded ships ~30 jars
# (Spring core/web/mvc + Jackson + SLF4J/Logback + Snakeyaml etc.).
lib_count="$(find "${lib_dir}" -maxdepth 1 -type f -name '*.jar' | wc -l | tr -d '[:space:]')"

echo "build-spring.sh: done — exploded WAR at ${exploded_dir} (${lib_count} jars under WEB-INF/lib/)"
