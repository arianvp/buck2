#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# End-to-end check of the install flow in a scratch copy of this project, so
# the test can edit a hook without touching the checked-in sources.

set -euo pipefail

work="$(mktemp -d)"
trap 'cd /; buck2 --isolation-dir=claude-hooks-test kill >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
tar -C "$(dirname "$0")" --exclude=buck-out --exclude=.claude/hooks -cf - . | tar -C "$work" -xf -
cd "$work"
export CLAUDE_PROJECT_DIR="$work"
export BUCK_ISOLATION_DIR=claude-hooks-test
hooks="$work/.claude/hooks"

run_hook() { # run_hook <name> <file_path>
    printf '{"cwd": "%s", "tool_input": {"file_path": "%s"}}' "$work" "$2" | "$hooks/$1"
}

echo "# Install" >&2
buck2 run //:claude_hooks --console=none
[[ -L "$hooks/protect_paths" && -L "$hooks/session_context" ]]
[[ "$("$hooks/session_context")" == *"builds with Buck2"* ]]

echo "# protect_paths allows sources and blocks generated files" >&2
run_hook protect_paths "$work/BUCK"
rc=0; run_hook protect_paths "$work/buck-out/v2/foo" 2>/dev/null || rc=$?
[[ $rc == 2 ]]

echo "# Hooks run from outside the project" >&2
(cd / && run_hook protect_paths "$work/BUCK")

echo "# A rebuild lands at a new path and leaves the running generation intact" >&2
old="$(readlink -f "$hooks/session_context")"
sed -i.bak 's/builds with Buck2/builds with Buck2 (v2)/' hooks/session_context.rs
buck2 build //:claude_hooks --console=none
[[ "$(readlink -f "$hooks/session_context")" == "$old" ]] # not installed yet
[[ "$("$hooks/session_context")" != *"(v2)"* ]]           # and still runnable

echo "# Reinstall swaps to the new generation; the old one still runs" >&2
buck2 run //:claude_hooks --console=none
[[ "$(readlink -f "$hooks/session_context")" != "$old" ]]
[[ "$("$hooks/session_context")" == *"(v2)"* ]]
[[ "$("$old")" != *"(v2)"* ]]

echo "# Hooks that are no longer declared are removed" >&2
ln -s nowhere "$hooks/stale_hook"
buck2 run //:claude_hooks --console=none
[[ ! -e "$hooks/stale_hook" && ! -L "$hooks/stale_hook" ]]

echo "OK" >&2
