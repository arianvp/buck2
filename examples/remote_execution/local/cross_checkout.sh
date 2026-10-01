#!/usr/bin/env bash
# Cross-machine cache check: build the same targets from two checkouts at
# different absolute paths, each with its own buck2 daemon and a fresh
# buck-out, sharing one RE backend (the harness). The second checkout should
# be served almost entirely from the action cache.
#
#   cross_checkout.sh <second checkout dir> <example>:<target>[,<target>...] ...
#
# e.g.  cross_checkout.sh /srv/machine-b go_mod://app:server npm://app:dist,//app:node_modules
#
# The second checkout is a `git clone` of this repository's HEAD. Run with an
# empty cache (`run.sh wipe && run.sh start`) for a clean measurement. Per
# example and checkout it prints re_stats.py's summary; full logs go to
# $OUT (default: a temp dir).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
second=$1
shift
OUT=${OUT:-$(mktemp -d)}

if [ ! -d "$second/.git" ]; then
    git clone -q "$repo" "$second"
fi
git -C "$second" fetch -q "$repo" HEAD
git -C "$second" checkout -q --detach FETCH_HEAD

build() {
    local checkout=$1 name=$2 example=$3 targets=$4
    local dir="$checkout/examples/$example"
    rm -rf "$dir/prelude"
    cp -r "$checkout/prelude" "$dir/prelude"
    cp "$here/re.buckconfig" "$dir/.buckconfig.local"
    (
        cd "$dir"
        buck2 kill >/dev/null 2>&1 || true
        buck2 clean >/dev/null 2>&1 || true
        # shellcheck disable=SC2086
        buck2 build ${targets//,/ } --console simple >"$OUT/$name-$example.build.log" 2>&1 \
            || { echo "build failed, see $OUT/$name-$example.build.log" >&2; exit 1; }
        buck2 log show >"$OUT/$name-$example.events.jsonl"
        buck2 kill >/dev/null 2>&1 || true
    )
    echo "== $name ($dir)"
    "$here/re_stats.py" --list <"$OUT/$name-$example.events.jsonl" | tee "$OUT/$name-$example.stats.txt" \
        | sed -n '/^command actions/,$p'
}

for spec in "$@"; do
    example=${spec%%:*}
    targets=${spec#*:}
    build "$repo" first "$example" "$targets"
    build "$second" second "$example" "$targets"
done
echo "logs: $OUT"
