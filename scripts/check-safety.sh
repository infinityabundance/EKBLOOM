#!/bin/sh
# SAFETY / ALLOCATION GATE — runs inside the pinned container (wired into
# scripts/container-verify.sh), never on the host.
#
# It enforces the crate's explicit safety and allocation story:
#
#   (a) print the `unsafe` inventory (`grep -rn 'unsafe' src/`) and FAIL if
#       `unsafe` *code* appears outside the three sanctioned modules
#       (`simd.rs`, `async_io.rs`, `mapped.rs`) — or if one of those modules
#       stops containing the `unsafe` it is documented to contain;
#   (b) count `.unwrap()` / `.expect(` in `src/` and report the totals, and FAIL
#       if the *non-test* (production) counts differ from the small allowlist
#       below (each production `expect` is a proven-infallible invariant — see
#       docs/ALLOC.md);
#   (c) confirm there is no `panic!` / `todo!` / `unimplemented!` in non-test
#       `src/`.
#
# Everything is read-only: the script only greps/awks the source tree.
set -eu

cd "$(dirname "$0")/.."

# The three sanctioned `unsafe` modules (see docs/SAFETY.md).
SANCTIONED="simd.rs async_io.rs mapped.rs"

# Small allowlist for *production* (non-test) panic-family constructs. These are
# the four proven-infallible `.expect(` invariants listed in docs/ALLOC.md; a
# change here must be re-audited, which is the point of the gate.
ALLOW_PROD_UNWRAP=0
ALLOW_PROD_EXPECT=4
ALLOW_PROD_PANIC=0

echo "== safety gate =="
echo "-- unsafe inventory (grep -rn 'unsafe' src/) --"
grep -rn 'unsafe' src/ || true
echo

# (a) Confinement. A line is `unsafe` code when the token appears on a
# non-comment line; this mirrors tests/mmap.rs::has_unsafe_code, so doc comments
# that merely *mention* `unsafe` (fastpath/reader/handle/machine_pack/kernels and
# the module headers) do not count.
unsafe_files=$(
    awk '
        {
            line = $0
            sub(/^[ \t]+/, "", line)
            if (line ~ /^\/\//) next
            if (line ~ /(^|[^[:alnum:]_])unsafe([^[:alnum:]_]|$)/) print FILENAME
        }
    ' src/*.rs | sed 's#^src/##' | sort -u
)

echo "-- modules containing unsafe code --"
echo "${unsafe_files:-<none>}"
echo

bad=""
for f in $unsafe_files; do
    case " $SANCTIONED " in
        *" $f "*) ;;
        *) bad="$bad $f" ;;
    esac
done
if [ -n "$bad" ]; then
    echo "FAIL: unsafe code outside the sanctioned modules:$bad" >&2
    exit 1
fi
for f in $SANCTIONED; do
    if ! printf '%s\n' "$unsafe_files" | grep -qx "$f"; then
        echo "FAIL: sanctioned module src/$f no longer contains its documented unsafe" >&2
        exit 1
    fi
done
echo "OK: unsafe code is confined to src/{simd,async_io,mapped}.rs"
echo

# (b) unwrap/expect counts.
total_unwrap=$(grep -rho --include='*.rs' '\.unwrap()' src/ | wc -l | tr -d ' ')
total_expect=$(grep -rho --include='*.rs' '\.expect(' src/ | wc -l | tr -d ' ')

prod=$(awk '
    FNR == 1 { flag = 0 }
    /^[[:space:]]*#\[cfg\(test\)\]/ { flag = 1 }
    !flag && /\.unwrap\(\)/ { u++ }
    !flag && /\.expect\(/ { e++ }
    !flag && /panic!|todo!|unimplemented!/ { p++ }
    END { printf "%s %s %s", u + 0, e + 0, p + 0 }
' src/*.rs)
prod_unwrap=$(printf '%s' "$prod" | cut -d' ' -f1)
prod_expect=$(printf '%s' "$prod" | cut -d' ' -f2)
prod_panic=$(printf '%s' "$prod" | cut -d' ' -f3)

echo "-- unwrap/expect counts (src/) --"
printf '  total        .unwrap() = %s\n' "$total_unwrap"
printf '  total        .expect(  = %s\n' "$total_expect"
printf '  non-test     .unwrap() = %s (allowlist %s)\n' "$prod_unwrap" "$ALLOW_PROD_UNWRAP"
printf '  non-test     .expect(  = %s (allowlist %s)\n' "$prod_expect" "$ALLOW_PROD_EXPECT"
echo

if [ "$prod_unwrap" != "$ALLOW_PROD_UNWRAP" ] || [ "$prod_expect" != "$ALLOW_PROD_EXPECT" ]; then
    echo "FAIL: production unwrap/expect counts differ from the audited allowlist" >&2
    exit 1
fi
echo "OK: production unwrap/expect counts match the audited allowlist"
echo

# (c) no panic!/todo!/unimplemented! in non-test src/.
if [ "$prod_panic" != "$ALLOW_PROD_PANIC" ]; then
    echo "FAIL: $prod_panic panic!/todo!/unimplemented! in non-test src/ (allowlist $ALLOW_PROD_PANIC)" >&2
    awk '
        FNR == 1 { flag = 0 }
        /^[[:space:]]*#\[cfg\(test\)\]/ { flag = 1 }
        !flag && /panic!|todo!|unimplemented!/ { print FILENAME ":" FNR ": " $0 }
    ' src/*.rs >&2
    exit 1
fi
echo "OK: no panic!/todo!/unimplemented! in non-test src/"
echo
echo "== safety gate OK =="
