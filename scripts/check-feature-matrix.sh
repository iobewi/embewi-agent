#!/usr/bin/env bash
# Runtime/feature guard: the production runtime is the native one; the probe is a test backend
# that cannot be combined with it; the fault-injection route exists only when asked for.
# Usage: scripts/check-feature-matrix.sh    (needs the `esp` toolchain)
set -uo pipefail
cd "$(dirname "$0")/.."
fail=0
expect() { # ok|fail <cargo args...>
    local want="$1"; shift
    if cargo +esp check --release --lib "$@" >/tmp/feature-matrix.$$ 2>&1; then got=ok; else got=fail; fi
    if [ "$got" = "$want" ]; then echo "  OK   [$want] $*"; else echo "  FAIL [$want, got $got] $*"; fail=1; fi
}
expect ok                                                                      # default = native runtime
expect ok   --no-default-features                                              # no Workload runtime at all
expect ok   --no-default-features --features test-probe-runtime               # the test backend, explicit
expect fail --features test-probe-runtime                                      # native + probe: compile_error
expect ok   --features test-fault-injection                                    # native + test route
cargo +esp check --release --lib --features test-probe-runtime >/tmp/feature-matrix-conflict.$$ 2>&1 || true
if grep -q "two different runtimes" /tmp/feature-matrix-conflict.$$; then
    echo "  OK   the conflict is reported by the explicit compile_error"
else
    echo "  FAIL the conflict is not the explicit compile_error"; fail=1
fi
rm -f /tmp/feature-matrix.$$ /tmp/feature-matrix-conflict.$$
exit $fail
