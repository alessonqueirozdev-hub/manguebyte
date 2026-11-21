#!/usr/bin/env bash
# Checks the layering rule of ADR 0001 and ADR 0007: no crate that
# processes audio may depend on an interface crate, directly or through
# anything else. Interface crates may depend on audio crates, never the
# other way round.
#
#   ./scripts/check-layering.sh [extra cargo tree flags, such as --offline]
#
# Every feature and every target is resolved, so a dependency behind a
# feature flag or a platform condition counts too. Only normal
# dependencies count: a test may use an interface crate, which a shipped
# library never sees.
set -euo pipefail
cd "$(dirname "$0")/.."

audio_crates=(
    manguebyte-core
    manguebyte-midi
    manguebyte-params
    manguebyte-state
    manguebyte-dsp
    manguebyte-formats
    manguebyte-osc
    manguebyte-analysis
    manguebyte-ump-io
    manguebyte-wasapi
    manguebyte-devices
    manguebyte-graph
    manguebyte-plugin
    manguebyte-clap
    manguebyte-vst3
    manguebyte-lv2
    manguebyte-standalone
    manguebyte-host
)

interface_crates=(
    manguebyte-gfx
    manguebyte-ui
    manguebyte-window
    manguebyte-gui
)

failed=0
for crate in "${audio_crates[@]}"; do
    # One line per dependency, starting "name vX.Y.Z". A crate that is not
    # in the workspace makes cargo fail, and so the script.
    tree="$(cargo tree "$@" -e normal -p "$crate" --all-features --target all \
        --prefix none --format '{p}')"
    for interface in "${interface_crates[@]}"; do
        if grep -Eq "^${interface} v" <<<"$tree"; then
            echo "error: ${crate} depends on the interface crate ${interface}:"
            cargo tree "$@" -e normal -p "$crate" --all-features --target all \
                --invert "$interface"
            failed=1
        fi
    done
    echo "ok: ${crate}"
done

if [ "$failed" -ne 0 ]; then
    echo "An audio crate depends on an interface crate (ADR 0007, decision 1)."
    exit 1
fi
echo "No audio crate depends on an interface crate."

# `manguebyte-pro-io` is under the GPLv3 alone (its README says why), and
# everything else under MIT OR Apache-2.0: no crate of the workspace may
# depend on it, in any way, for tests and builds included, or the GPLv3
# would reach it. The inverted tree lists the crate and its dependents.
dependents="$(cargo tree "$@" --workspace --all-features --target all -e normal,build,dev \
    --invert manguebyte-pro-io --prefix none --format '{p}' | sort -u \
    | grep -v '^manguebyte-pro-io v' || true)"
if [ -n "$dependents" ]; then
    echo "error: these crates depend on manguebyte-pro-io, which is under the GPLv3:"
    echo "$dependents"
    exit 1
fi
echo "Nothing depends on manguebyte-pro-io."
