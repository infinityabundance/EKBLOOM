#!/bin/sh
# Hardware performance counters for the EKBLOOM 1-byte and 4 KiB read workloads.
#
# Wraps `examples/perf_target.rs` in `perf stat` inside a container granted the
# single capability `perf_event_open` needs (the CAP_PERFMON pattern from
# scripts/container-perf.sh). Events:
#   cycles, instructions, cache-references, cache-misses, branches,
#   branch-misses, L1-dcache-load-misses.
# If the kernel/PMU cannot provide L1-dcache-load-misses the remaining counters
# are still reported and `l1_available` is false — nothing is fabricated.
#
# Reports, for the single-byte (`random-1b`) and 4 KiB (`range-4k`) workloads:
#   instructions per byte, instructions/cycles per read, cache-miss rate,
#   branch-miss rate, and L1 miss rate.
#
# Output: evidence/bench/counters.json. Nothing here runs project tooling on the
# host.
#
# Run: sh scripts/run-counters.sh
set -eu

EKB_MEM_LIMIT=6g
EKB_MEMSWAP_LIMIT=6g
export EKB_MEM_LIMIT EKB_MEMSWAP_LIMIT

. "$(dirname "$0")/lib.sh"

PERF_IMAGE="ekbloom-perf:1.99.0"
OUT="$PROJECT_DIR/evidence/bench/counters.json"
PERF_BIN="/work/target/release/examples/perf_target"
FULL_EVENTS="cycles,instructions,cache-references,cache-misses,branches,branch-misses,L1-dcache-load-misses"

mkdir -p "$PROJECT_DIR/evidence/bench"

# --- 1. build the example with the normal, capability-free harness --------------
echo "== build example perf_target =="
docker_run cargo build --release --example perf_target

# --- 2. ensure the perf-capable image exists ------------------------------------
if ! docker image inspect "$PERF_IMAGE" >/dev/null 2>&1; then
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
    rm -rf "$BUILD_DIR"
    trap - EXIT INT TERM
fi

# --- 3. run a command in the perf container with a given capability -------------
run_perf() {
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

PARANOID="$(docker_run cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unavailable)"
PARANOID="$(printf '%s' "$PARANOID" | tr -d '[:space:]')"
KERNEL="$(docker_run uname -r 2>/dev/null || echo unavailable)"
KERNEL="$(printf '%s' "$KERNEL" | tr -d '[:space:]')"

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

if [ -z "$CAPABILITY" ]; then
    echo "== perf counters unavailable; writing truthful report =="
    {
        printf '{\n'
        printf '  "generated_by": "scripts/run-counters.sh",\n'
        printf '  "counters_available": false,\n'
        printf '  "reason": "perf_event_open denied for every capability tried (perf_event_paranoid=%s)",\n' "$PARANOID"
        printf '  "capability_attempts": [%s],\n' "$ATTEMPTS_JSON"
        printf '  "perf_error": "%s",\n' "$(json_str "$PROBE_ERROR")"
        printf '  "host_kernel": "%s",\n' "$KERNEL"
        printf '  "events_requested": "%s"\n' "$FULL_EVENTS"
        printf '}\n'
    } > "$OUT"
    cat "$OUT"
    exit 0
fi

PERF_VERSION="$(run_perf "$CAPABILITY" 'perf --version 2>&1' | tr -d '\r' | head -n 1)"
[ -n "$PERF_VERSION" ] || PERF_VERSION="unknown"
echo "capability required: $CAPABILITY (perf_event_paranoid=$PARANOID, $PERF_VERSION)"

# --- 5. measure the two workloads ----------------------------------------------
OBJECTS=""
for wl in random-1b range-4k; do
    case "$wl" in
        random-1b) ITERS=2000; READS=$((2000 * 1048576)) ;;
        range-4k)  ITERS=256;  READS=$((256 * 65536)) ;;
    esac
    echo "== perf stat: $wl ($ITERS iterations, path(D=4)) =="
    cmd="perf stat -x, -o /tmp/perf.csv -e $FULL_EVENTS -- $PERF_BIN $wl $ITERS path4; rc=\$?; echo '===CSV==='; cat /tmp/perf.csv; exit 0"
    raw="$(run_perf "$CAPABILITY" "$cmd" 2>&1)"
    obj="$(printf '%s\n' "$raw" | awk -v wl="$wl" -v reads="$READS" '
        BEGIN { FS = ","; csv = 0 }
        /^workload=/     { w = substr($0, index($0, "=") + 1) }
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
            l1 = val["L1-dcache-load-misses"];
            ipb = (ins != "" && bytes + 0 > 0) ? sprintf("%.5f", ins / bytes) : "null";
            ipr = (ins != "" && reads + 0 > 0) ? sprintf("%.4f", ins / reads) : "null";
            cpr = (cyc != "" && reads + 0 > 0) ? sprintf("%.4f", cyc / reads) : "null";
            cmr = (cr != "" && cm != "" && cr + 0 > 0) ? sprintf("%.6f", cm / cr) : "null";
            bmr = (br != "" && bm != "" && br + 0 > 0) ? sprintf("%.6f", bm / br) : "null";
            l1r = (l1 != "" && reads + 0 > 0) ? sprintf("%.5f", l1 / reads) : "null";
            printf "    {\n";
            printf "      \"workload\": \"%s\",\n", wl;
            printf "      \"profile\": \"%s\",\n", prof;
            printf "      \"iterations\": %s,\n", (iters == "" ? "0" : iters);
            printf "      \"reads\": %d,\n", reads;
            printf "      \"logical_bytes\": %s,\n", (bytes == "" ? "null" : bytes);
            printf "      \"seconds\": %s,\n", (sec == "" ? "null" : sec);
            printf "      \"instructions_per_byte\": %s,\n", ipb;
            printf "      \"instructions_per_read\": %s,\n", ipr;
            printf "      \"cycles_per_read\": %s,\n", cpr;
            printf "      \"cycles\": %s,\n", (cyc == "" ? "null" : cyc);
            printf "      \"instructions\": %s,\n", (ins == "" ? "null" : ins);
            printf "      \"cache_references\": %s,\n", (cr == "" ? "null" : cr);
            printf "      \"cache_misses\": %s,\n", (cm == "" ? "null" : cm);
            printf "      \"cache_miss_rate\": %s,\n", cmr;
            printf "      \"branches\": %s,\n", (br == "" ? "null" : br);
            printf "      \"branch_misses\": %s,\n", (bm == "" ? "null" : bm);
            printf "      \"branch_miss_rate\": %s,\n", bmr;
            printf "      \"l1_dcache_load_misses\": %s,\n", (l1 == "" ? "null" : l1);
            printf "      \"l1_misses_per_read\": %s\n", l1r;
            printf "    }";
        }')"
    if [ -z "$OBJECTS" ]; then
        OBJECTS="$obj"
    else
        OBJECTS="$OBJECTS,
$obj"
    fi
done

L1_AVAILABLE=false
case "$OBJECTS" in
    *'"l1_dcache_load_misses": null'*) L1_AVAILABLE=false ;;
    *) L1_AVAILABLE=true ;;
esac

{
    printf '{\n'
    printf '  "generated_by": "scripts/run-counters.sh",\n'
    printf '  "counters_available": true,\n'
    printf '  "capability": "%s",\n' "$CAPABILITY"
    printf '  "capability_attempts": [%s],\n' "$ATTEMPTS_JSON"
    printf '  "perf_event_paranoid": %s,\n' "$(case "$PARANOID" in ''|*[!0-9-]*) printf '"%s"' "$PARANOID" ;; *) printf '%s' "$PARANOID" ;; esac)"
    printf '  "perf_version": "%s",\n' "$PERF_VERSION"
    printf '  "host_kernel": "%s",\n' "$KERNEL"
    printf '  "events_requested": "%s",\n' "$FULL_EVENTS"
    printf '  "l1_available": %s,\n' "$L1_AVAILABLE"
    printf '  "source_len": 1048576,\n'
    printf '  "source_seed": "0x0EB0123456789ABC",\n'
    printf '  "access_seed": "0x9E3779B97F4A7C15",\n'
    printf '  "profile": "path(D=4)",\n'
    printf '  "workloads": [\n%s\n  ]\n' "$OBJECTS"
    printf '}\n'
} > "$OUT"

echo "wrote $OUT"
cat "$OUT"
