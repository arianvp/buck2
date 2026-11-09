# Claude Code hooks as Buck2 targets

This example writes [Claude Code hooks](https://code.claude.com/docs/en/hooks)
as ordinary Buck2 runnables, and installs them so that running a hook never
involves the Buck2 daemon.

The obvious approach, `"command": "buck2 run //:my_hook"`, tends to hang.
Claude Code runs matching hooks in parallel while Claude edits files, so each
invocation reaches the daemon with a different file state. Buck2 cannot run
commands with different states at the same time, so they queue up. Here the
daemon is used once per session, in the background, and hooks run a path in
`buck-out`.

## How it works

```
BUCK                       claude_hooks(hooks = {"protect_paths": ":protect_paths", ...})
  │  buck2 run //:claude_hooks        (async SessionStart hook)
  ▼
buck-out/v2/art/root/__claude_hooks__/<content hash>/protect_paths     launcher
  │  relative path, resolved against the launcher's own location
  ▼
buck-out/v2/art/root/__protect_paths__/<content hash>/protect_paths    the hook

.claude/hooks/protect_paths ──symlink──▶ the launcher        (what settings.json runs)
```

- **Hooks are plain targets.** `protect_paths` is an `sh_binary` running a
  Python script and `session_context` is a `rust_binary`. Anything with a
  `RunInfo` works.
- **`claude_hooks` wraps each one in a launcher.** It uses the prelude's
  `command_alias`, whose launchers find everything relative to their own
  location, so they can be run through a symlink from outside `buck-out`.
- **Everything is content-based.** A content-based output's path contains a
  hash of its contents. When you change a hook, the rebuilt hook and its
  launcher land at new paths, and the files a running hook is using are never
  rewritten. `sh_binary` is content-based by default; `rust_binary` needs
  `has_content_based_path = True`.
- **Installing is `buck2 run //:claude_hooks`.** The target's `RunInfo` is an
  installer that points `.claude/hooks/<name>` at each launcher, replacing each
  symlink with a rename. A hook that fires during a reinstall runs either the
  old version or the new one, never a missing or half-written file.
- **Claude Code does the scheduling.** `.claude/settings.json` reinstalls in an
  `async` SessionStart hook, so a session never waits on Buck2. Hooks run the
  previously installed version until the new one is ready. In a fresh checkout
  the hooks do nothing until the first install finishes.

The install runs on the default daemon with `--preemptible=ondifferentstate`:
it shares the build cache with your normal builds, and if you or Claude start a
build against different files, that build cancels the install instead of
waiting for it. The hooks stay on the previous version until the next session.

## Try it

```sh
buck2 run //:claude_hooks    # install into .claude/hooks
claude                       # SessionStart context comes from :session_context
```

Ask Claude to edit something under `buck-out/` and `protect_paths` blocks it.

To test a hook without installing, run its launcher directly:

```sh
echo '{"tool_input": {"file_path": "buck-out/x"}}' | buck2 run '//:claude_hooks[protect_paths]'
```

`./test.sh` checks the whole flow, including that a rebuilt hook doesn't
disturb the installed one.

## Adding a hook

1. Add a runnable target with content-based outputs.
2. Add it to `hooks` in `claude_hooks`.
3. Reference `$CLAUDE_PROJECT_DIR/.claude/hooks/<name>` from
   `.claude/settings.json`, guarded with `test -x ... || exit 0` for the first
   session in a new checkout.

## Limitations

- **Config-based runnables are not immune to rebuilds.** They install and run
  fine, but a rebuild can change their files under a running hook. That
  includes `python_binary` with the default `inplace` packaging (its `.par`
  launcher and link tree are config-based); run Python scripts through
  `sh_binary` as this example does.
- **Unix only.** The installer creates symlinks and the launchers are bash.
- **Old versions stay in `buck-out`** until `buck2 clean --stale` removes them.
  Each session start reinstalls, which rebuilds or refreshes the current
  version, so the installed paths are never the stale ones.
