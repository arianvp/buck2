# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

"""Claude Code hooks as ordinary Buck2 runnables.

A hook is any target with a `RunInfo`. `claude_hooks` wraps each one in a
launcher and makes the whole set installable with `buck2 run`:

    buck2 run //:claude_hooks

The installer points `.claude/hooks/<name>` at each launcher in `buck-out`.
Hooks then execute those paths directly and never talk to the Buck2 daemon, so
they can't block behind (or be blocked by) a build.

What makes this safe is content-based paths: the launcher's output path is a
hash of its contents, and its contents name the hook's own content-based path.
A rebuild that changes a hook therefore lands at a *new* path and never
rewrites a file a running hook is using. The previous generation keeps working
until the installer atomically swaps the symlink over, which is what gives
Claude Code a "stale but never broken" hook while a rebuild is in flight.

This only holds if the hook's runtime files are content-based too.
`sh_binary` is by default, `rust_binary` is with `has_content_based_path =
True`. Config-based runnables (e.g. an `inplace` `python_binary`) still work,
but a rebuild can change their files under a running hook.
"""

load("@prelude//:command_alias.bzl", "command_alias")
load("@prelude//os_lookup:defs.bzl", "OsLookup")

def _claude_hooks_impl(ctx: AnalysisContext) -> list[Provider]:
    target_os = ctx.attrs._target_os_type[OsLookup]

    launchers = []
    install_args = cmd_args()
    sub_targets = {}
    for name, hook in ctx.attrs.hooks.items():
        # A relocatable trampoline around the hook's `RunInfo`. It refers to
        # everything relative to its own location, so it can be run through a
        # symlink from outside `buck-out`, and it is content-based so it is
        # immutable once written.
        launcher = command_alias(
            actions = ctx.actions,
            path = name,
            target_os = target_os,
            base = hook[RunInfo],
            args = cmd_args(),
            env = {},
            labels = [],
            has_content_based_path = True,
        )
        launchers.append(launcher.output)
        install_args.add("--hook", name, launcher.cmd)
        sub_targets[name] = [launcher.output, RunInfo(args = launcher.cmd)]

    return [
        DefaultInfo(
            default_outputs = [o.default_outputs[0] for o in launchers],
            other_outputs = [x for o in launchers for x in o.other_outputs],
            sub_targets = sub_targets,
        ),
        RunInfo(args = cmd_args(ctx.attrs._installer[RunInfo], install_args)),
    ]

claude_hooks = rule(
    impl = _claude_hooks_impl,
    attrs = {
        "hooks": attrs.dict(
            attrs.string(),
            attrs.dep(providers = [RunInfo]),
            doc = "Hook name (the file name under `.claude/hooks/`) to the runnable implementing it.",
        ),
        "_installer": attrs.default_only(attrs.exec_dep(default = "root//:install_claude_hooks", providers = [RunInfo])),
        "_target_os_type": attrs.default_only(attrs.dep(default = "prelude//os_lookup/targets:os_lookup")),
    },
    doc = "Installs a set of Claude Code hooks, see the module docstring.",
)
