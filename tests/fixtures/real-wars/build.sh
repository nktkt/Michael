#!/usr/bin/env bash
# Compile every real-WAR fixture under `tests/fixtures/real-wars/` into its
# own `WEB-INF/classes/` tree.
#
# Run manually after editing a fixture's Java sources:
#     bash tests/fixtures/real-wars/build.sh
#
# CI invokes this once before running the JVM-bridge integration test
# (`tomcatrs-webapp --test real_war`), which then opens the resulting webapp
# directories with `Webapp::open` and asserts the class files are present.
#
# Behaviour:
#   * Idempotent: re-compiles in place; safe to run repeatedly.
#   * Self-locating: works from any CWD, on macOS and Linux, with or without
#     a git checkout (falls back to a relative path from this file).
#   * Defensive: bails with a clear, non-zero-exit error if `javac` is not on
#     PATH instead of failing silently. Honours `JAVA_HOME` if set.
#   * Scoped: only operates on direct sub-directories of `real-wars/` that
#     have a `WEB-INF/src/` tree; the `_stubs/` support directory is skipped.
#
# Exit codes:
#   0   every fixture compiled (or there was nothing to compile)
#   1   `javac` missing or compile failure
#   2   layout error (could not locate the fixtures dir or jakarta-stubs dir)

set -euo pipefail

# --- Self-locate ------------------------------------------------------------

# Resolve the directory containing this script (real-wars/), portably on macOS
# (no GNU `readlink -f`) and Linux. Used both as the fixtures root and as the
# anchor for the relative path to the bridge stubs.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixtures_dir="${script_dir}"

# Locate the workspace root. Prefer `git rev-parse` when available so the
# script works from any worktree layout; otherwise climb two levels up from
# `tests/fixtures/real-wars/` to reach the workspace root.
if command -v git >/dev/null 2>&1 \
   && workspace_root="$(git -C "${script_dir}" rev-parse --show-toplevel 2>/dev/null)"; then
    :
else
    workspace_root="$(cd "${script_dir}/../../.." && pwd)"
fi

bridge_stubs_dir="${workspace_root}/crates/tomcatrs-servlet-bridge/java/jakarta-stubs"
fixture_stubs_dir="${fixtures_dir}/_stubs"

if [[ ! -d "${fixtures_dir}" ]]; then
    echo "build.sh: fixtures dir ${fixtures_dir} not found" >&2
    exit 2
fi
if [[ ! -d "${bridge_stubs_dir}" ]]; then
    echo "build.sh: bridge jakarta-stubs not found at ${bridge_stubs_dir}" >&2
    echo "build.sh: (workspace root resolved to ${workspace_root})" >&2
    exit 2
fi
if [[ ! -d "${fixture_stubs_dir}" ]]; then
    echo "build.sh: fixture stubs not found at ${fixture_stubs_dir}" >&2
    exit 2
fi

# --- Locate javac -----------------------------------------------------------

if [[ -n "${JAVA_HOME:-}" && -x "${JAVA_HOME}/bin/javac" ]]; then
    javac_bin="${JAVA_HOME}/bin/javac"
elif command -v javac >/dev/null 2>&1; then
    javac_bin="$(command -v javac)"
else
    cat >&2 <<'EOF'
build.sh: `javac` not found on PATH (and $JAVA_HOME/bin/javac is unset or
          missing).

This script compiles real, runnable Java servlet sources used by the
JVM-bridge integration tests. Install a JDK (any JDK 17+ works), or set
$JAVA_HOME to point at one, and re-run.

On macOS:    brew install openjdk    (then read the caveats it prints)
On Debian:   apt-get install default-jdk
On RHEL:     dnf install java-21-openjdk-devel
EOF
    exit 1
fi

echo "build.sh: using $("${javac_bin}" -version 2>&1)"

# --- Pre-compile the fixture stubs to a temp class dir ----------------------
#
# The fixture stubs live as .java sources under `_stubs/`; we compile them
# once into a temp directory and put that on `-cp` when compiling each
# fixture. That keeps the fixture WEB-INF/classes/ trees clean of any
# `jakarta/servlet/*.class` files — a real production WAR would never ship
# those, and the integration tests assert only the application classes are
# emitted into WEB-INF/classes.

stubs_classes_dir="$(mktemp -d "${TMPDIR:-/tmp}/tomcatrs-realwar-stubs-XXXXXX")"
trap 'rm -rf "${stubs_classes_dir}"' EXIT

fixture_stub_sources=()
while IFS= read -r -d '' f; do
    fixture_stub_sources+=("${f}")
done < <(find "${fixture_stubs_dir}" -type f -name '*.java' -print0)

if [[ ${#fixture_stub_sources[@]} -eq 0 ]]; then
    echo "build.sh: no fixture stub sources under ${fixture_stubs_dir}" >&2
    exit 2
fi

"${javac_bin}" \
    -d "${stubs_classes_dir}" \
    -encoding UTF-8 \
    "${fixture_stub_sources[@]}"

# --- Compile each fixture ---------------------------------------------------

# Track how many fixtures had compilable sources so the developer sees what
# was (and wasn't) built.
built_count=0
skipped_count=0

for war_dir in "${fixtures_dir}"/*/; do
    war_dir="${war_dir%/}"          # strip trailing slash
    war_name="$(basename "${war_dir}")"

    # Skip the stubs support directory and any dotfile-style sidecars.
    case "${war_name}" in
        _*) continue ;;
    esac

    src_dir="${war_dir}/WEB-INF/src"
    classes_dir="${war_dir}/WEB-INF/classes"

    if [[ ! -d "${src_dir}" ]]; then
        echo "build.sh: ${war_name}: no WEB-INF/src/, skipping"
        skipped_count=$((skipped_count + 1))
        continue
    fi

    # Gather every .java source under this fixture (NUL-delimited for safety
    # against unusual file names — even though our fixtures use plain ASCII).
    sources=()
    while IFS= read -r -d '' f; do
        sources+=("${f}")
    done < <(find "${src_dir}" -type f -name '*.java' -print0)

    if [[ ${#sources[@]} -eq 0 ]]; then
        echo "build.sh: ${war_name}: WEB-INF/src/ has no .java files, skipping"
        skipped_count=$((skipped_count + 1))
        continue
    fi

    # Recreate the classes dir to ensure idempotent, stale-free output. The
    # `.gitkeep` (if any) is recreated so empty fixture trees stay tracked.
    rm -rf "${classes_dir}"
    mkdir -p "${classes_dir}"

    echo "build.sh: ${war_name}: compiling ${#sources[@]} source(s) → ${classes_dir}"

    # Classpath: the pre-compiled fixture stubs only. The bridge stubs are
    # NOT on the classpath — the bridge declares only the surface its facades
    # implement, and the fixture stubs intentionally provide a richer (real-
    # API-shaped) surface so application servlets can extend `HttpServlet`,
    # implement listeners and filters, etc.
    "${javac_bin}" \
        -d "${classes_dir}" \
        -encoding UTF-8 \
        -cp "${stubs_classes_dir}" \
        "${sources[@]}"

    built_count=$((built_count + 1))
done

echo "build.sh: done — ${built_count} built, ${skipped_count} skipped"

# Echo the bridge stubs path so downstream tooling can grep it out if needed.
echo "build.sh: bridge jakarta-stubs at ${bridge_stubs_dir}"
