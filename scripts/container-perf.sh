#!/bin/sh
# Hardware performance counters for representative EKBLOOM workloads (directive §25).
#
# Wraps `examples/perf_target.rs` in `perf stat` inside a container granted the
# single capability `perf_event_open` needs. Capabilities are tried in the
# mandated order — CAP_PERFMON, then CAP_SYS_ADMIN, and clearly-labelled LAST
# resort `--privileged` — and the capability actually required is recorded.
#
# Output: evidence/bench/perf.json. Nothing is fabricated: when counters cannot be
# read, the JSON records the exact `perf_event_paranoid` value and the exact perf
# error instead of numbers.
#
# Hard rule: all tooling runs inside the container, never on the host.
#
# Run: sh scripts/container-perf.sh
set -eu
. "$(dirname "$0")/lib.sh"

PERF_IMAGE="ekbloom-perf:1.99.0"
OUT="$PROJECT_DIR/evidence/bench/perf.json"
PERF_BIN="/work/target/release/examples/perf_target"
EVENTS="cycles,instructions,cache-references,cache-misses,branches,branch-misses,context-switches,page-faults"

mkdir -p "$PROJECT_DIR/evidence/bench"

# --- 1. build the example with the normal, capability-free harness --------------
# (the target directory therefore stays owned by the invoking host user)
echo "== build example perf_target =="
docker_run cargo build --release --example perf_target

# --- 2. build the perf-capable image on top of the pinned project image ---------
# Debian bookworm's perf tool package is `linux-perf`; if it is ever absent we try
# the kernel-matched package and the `linux-tools` equivalent, then fail loudly.
echo "== build perf image $PERF_IMAGE =="
BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$BUILD_DIR"' EXIT INT TERM
{
    echo "FROM $IMAGE"
    echo "USER root"
    echo 'RUN set -eux; \'
    echo '    apt-get update; \'
    echo '    apt-get install -y --no-install-recommends linux-perf || true; \'
    echo '    if ! command -v perf >/dev/null 2>&1; then \'
    echo '      apt-get install -y --no-install-recommends "linux-perf-$(uname -r)" || \'
    echo '      apt-get install -y --no-install-recommends linux-tools-generic || true; \'
    echo '    fi; \'
    echo '    rm -rf /var/lib/apt/lists/*; \'
    echo '    command -v perf; perf --version'
} > "$BUILD_DIR/Dockerfile"
docker build -t "$PERF_IMAGE" "$BUILD_DIR"

# --- 3. run a command in the perf container with a given capability -------------
# Runs as root because Docker does not grant added capabilities to a non-root
# process; nothing is written back into the checkout by this container.
run_perf() {
    # $1 = capability mode (PERFMON | SYS_ADMIN | PRIVILEGED); $2 = shell command
    case "$1" in
        PERFMON)    CAPFLAGS="--cap-drop=ALL --cap-add=PERFMON" ;;
        SYS_ADMIN)  CAPFLAGS="--cap-drop=ALL --cap-add=SYS_ADMIN" ;;
        PRIVILEGED) CAPFLAGS="--privileged" ;;
        *)          echo "internal error: bad capability '$1'" >&2; exit 1 ;;
    esac
    # shellcheck disable=SC2086
    docker run --rm \
        --memory="$EKB_MEM_LIMIT" \
        --memory-swap="$EKB_MEMSWAP_LIMIT" \
        --cpus="$EKB_CPUS" \
        --pids-limit="$EKB_PIDS" \
        --ulimit core=0 \
        $CAPFLAGS \
        -v "$PROJECT_DIR":/work \
        -w /work \
        -u 0:0 \
        -e HOME=/tmp \
        "$PERF_IMAGE" sh -c "$2"
}

# --- provenance -----------------------------------------------------------------
PARANOID="$(docker_run cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unavailable)"
PARANOID="$(printf '%s' "$PARANOID" | tr -d '[:space:]')"
KERNEL="$(docker_run uname -r 2>/dev/null || echo unavailable)"
KERNEL="$(printf '%s' "$KERNEL" | tr -d '[:space:]')"

# A JSON string literal from arbitrary text (escapes backslash/quote/tab, turns
# newlines into `\n`). The text itself is preserved.
json_str() {
    printf '%s' "$1" | awk '
        BEGIN { ORS = ""; first = 1 }
        {
            if (!first) printf "\\n";
            first = 0;
            line = $0;
            gsub(/\\/, "\\\\", line);
            gsub(/"/, "\\\"", line);
            gsub(/\t/, "\\t", line);
            printf "%s", line;
        }'
}

# --- 4. probe which capability is required, in the mandated order ---------------
PROBE_CMD='perf stat -e cycles,instructions -- true'
CAPABILITY=""
PROBE_ERROR=""
ATTEMPTS_JSON=""
for mode in PERFMON SYS_ADMIN PRIVILEGED; do
    ATTEMPTS_JSON="${ATTEMPTS_JSON}${ATTEMPTS_JSON:+, }\"$mode\""
    echo "== probing capability: $mode =="
    if probe_out="$(run_perf "$mode" "$PROBE_CMD" 2>&1)"; then
        CAPABILITY="$mode"
        echo "capability $mode enables perf counters"
        break
    fi
    PROBE_ERROR="$probe_out"
    echo "capability $mode did NOT enable perf counters"
done

# --- 5a. truthful report when counters are unavailable --------------------------
if [ -z "$CAPABILITY" ]; then
    echo "== perf counters unavailable; writing truthful report =="
    REASON="perf_event_open denied for every capability tried (perf_event_paranoid=$PARANOID)"
    {
        printf '{\n'
        printf '  "generated_by": "scripts/container-perf.sh",\n'
        printf '  "counters_available": false,\n'
        printf '  "reason": "%s",\n' "$REASON"
        printf '  "perf_event_paranoid": %s,\n' "$(case "$PARANOID" in ''|*[!0-9-]*) printf '"%s"' "$PARANOID" ;; *) printf '%s' "$PARANOID" ;; esac)"
        printf '  "capability_attempts": [%s],\n' "$ATTEMPTS_JSON"
        printf '  "perf_error": "%s",\n' "$(json_str "$PROBE_ERROR")"
        printf '  "host_kernel": "%s",\n' "$KERNEL"
        printf '  "what_was_measured_instead": "wall-clock latency/throughput only; see docs/RESULTS.md"\n'
        printf '}\n'
    } > "$OUT"
    echo "wrote $OUT (counters unavailable)"
    cat "$OUT"
    exit 0
fi

# --- 5b. measure the real workloads ---------------------------------------------
PERF_VERSION="$(run_perf "$CAPABILITY" 'perf --version 2>&1' | tr -d '\r' | head -n 1)"
[ -n "$PERF_VERSION" ] || PERF_VERSION="unknown"

echo "capability required: $CAPABILITY (perf_event_paranoid=$PARANOID, $PERF_VERSION)"

OBJECTS=""
for wl in seq random-1b range-4k; do
    case "$wl" in
        seq)       ITERS=120000 ;;
        random-1b) ITERS=2000 ;;
        range-4k)  ITERS=256 ;;
    esac
    echo "== perf stat: $wl ($ITERS iterations, path(D=4)) =="
    cmd="perf stat -x, -o /tmp/perf.csv -e $EVENTS -- $PERF_BIN $wl $ITERS path4; rc=\$?; echo '===CSV==='; cat /tmp/perf.csv; exit \$rc"
    raw="$(run_perf "$CAPABILITY" "$cmd")"
    obj="$(printf '%s\n' "$raw" | awk '
        BEGIN { FS = ","; csv = 0 }
        /^workload=/     { wl = substr($0, index($0, "=") + 1) }
        /^profile=/      { prof = substr($0, index($0, "=") + 1) }
        /^iterations=/   { iters = substr($0, index($0, "=") + 1) }
        /^logical_bytes=/{ bytes = substr($0, index($0, "=") + 1) }
        /^seconds=/      { sec = substr($0, index($0, "=") + 1) }
        /^===CSV===/     { csv = 1; next }
        csv && $0 !~ /^#/ && NF >= 3 {
            if ($1 ~ /^</) val[$3] = "";
            else val[$3] = $1;
        }
        END {
            cyc = val["cycles"]; ins = val["instructions"];
            cr = val["cache-references"]; cm = val["cache-misses"];
            br = val["branches"]; bm = val["branch-misses"];
            cs = val["context-switches"]; pf = val["page-faults"];
            ipc = (cyc != "" && ins != "" && cyc + 0 > 0) ? sprintf("%.6g", ins / cyc) : "null";
            cmr = (cr != "" && cm != "" && cr + 0 > 0) ? sprintf("%.6g", cm / cr) : "null";
            bmr = (br != "" && bm != "" && br + 0 > 0) ? sprintf("%.6g", bm / br) : "null";
            bps = (bytes != "" && sec != "" && sec + 0 > 0) ? sprintf("%.9g", bytes / sec) : "null";
            printf "    {\n";
            printf "      \"workload\": \"%s\",\n", wl;
            printf "      \"profile\": \"%s\",\n", prof;
            printf "      \"iterations\": %s,\n", (iters == "" ? "0" : iters);
            printf "      \"seconds\": %s,\n", (sec == "" ? "null" : sec);
            printf "      \"logical_bytes\": %s,\n", (bytes == "" ? "null" : bytes);
            printf "      \"bytes_per_sec\": %s,\n", bps;
            printf "      \"cycles\": %s,\n", (cyc == "" ? "null" : cyc);
            printf "      \"instructions\": %s,\n", (ins == "" ? "null" : ins);
            printf "      \"ipc\": %s,\n", ipc;
            printf "      \"cache_references\": %s,\n", (cr == "" ? "null" : cr);
            printf "      \"cache_misses\": %s,\n", (cm == "" ? "null" : cm);
            printf "      \"cache_miss_rate\": %s,\n", cmr;
            printf "      \"branches\": %s,\n", (br == "" ? "null" : br);
            printf "      \"branch_misses\": %s,\n", (bm == "" ? "null" : bm);
            printf "      \"branch_miss_rate\": %s,\n", bmr;
            printf "      \"context_switches\": %s,\n", (cs == "" ? "null" : cs);
            printf "      \"page_faults\": %s\n", (pf == "" ? "null" : pf);
            printf "    }";
        }')"
    if [ -z "$OBJECTS" ]; then
        OBJECTS="$obj"
    else
        OBJECTS="$OBJECTS,
$obj"
    fi
done

{
    printf '{\n'
    printf '  "generated_by": "scripts/container-perf.sh",\n'
    printf '  "counters_available": true,\n'
    printf '  "capability": "%s",\n' "$CAPABILITY"
    printf '  "capability_attempts": [%s],\n' "$ATTEMPTS_JSON"
    printf '  "perf_event_paranoid": %s,\n' "$(case "$PARANOID" in ''|*[!0-9-]*) printf '"%s"' "$PARANOID" ;; *) printf '%s' "$PARANOID" ;; esac)"
    printf '  "perf_version": "%s",\n' "$PERF_VERSION"
    printf '  "host_kernel": "%s",\n' "$KERNEL"
    printf '  "events_requested": "%s",\n' "$EVENTS"
    printf '  "source_len": 1048576,\n'
    printf '  "source_seed": "0x0EB0123456789ABC",\n'
    printf '  "access_seed": "0x9E3779B97F4A7C15",\n'
    printf '  "profile": "path(D=4)",\n'
    printf '  "workloads": [\n%s\n  ]\n' "$OBJECTS"
    printf '}\n'
} > "$OUT"

echo "wrote $OUT"
cat "$OUT"
