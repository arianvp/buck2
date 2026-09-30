# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict

# End-to-end tests of `buck2 repl`, run non-interactively (inputs from `-e` or stdin).

import json
from pathlib import Path
from typing import Any, Dict, List, Optional

from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.api.buck_result import ExitCodeV2
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test


def _json_lines(stdout: str) -> List[Dict[str, Any]]:
    return [json.loads(line) for line in stdout.splitlines()]


# Completion answers once the daemon has loaded what it needs (packages, modules), however
# long that takes on a slow machine.
_COMPLETION_ENV = {"BUCK2_REPL_COMPLETION_TIMEOUT_MS": "120000"}


def _complete_input(buf: str) -> str:
    return ":__complete " + json.dumps({"buf": buf})


def _replacements(answer: Dict[str, Any]) -> List[str]:
    return [c["replacement"] for c in answer["candidates"]]


async def _complete(
    buck: Buck, *bufs: str, rel_cwd: Optional[Path] = None
) -> List[List[str]]:
    """The replacements `:__complete` offers for each buffer (completed at its end)."""
    args = [arg for buf in bufs for arg in ("-e", _complete_input(buf))]
    result = await buck.repl(*args, rel_cwd=rel_cwd, env=_COMPLETION_ENV)
    answers = _json_lines(result.stdout)
    assert len(answers) == len(bufs), result.stdout
    for answer in answers:
        assert answer["status"] == "ok", answer
    return [_replacements(answer) for answer in answers]


# Evaluation (inputs, values, errors, limits).


@buck_test()
async def test_repl_stdin(buck: Buck) -> None:
    result = await buck.repl(input=b"x = 1\nx + 1\n")
    assert result.stdout == "2\n"

    result = await buck.repl(input=b"def f(n):\n    return n * 3\nf(4)\n")
    assert result.stdout == "12\n"


@buck_test()
async def test_repl_eval_args(buck: Buck) -> None:
    result = await buck.repl("-e", "'a'", "-e", "'b'")
    assert result.stdout == '"a"\n"b"\n'


@buck_test()
async def test_repl_ctx(buck: Buck) -> None:
    result = await buck.repl("-e", 'ctx.unconfigured_targets("//:hello")')
    assert "root//:hello" in result.stdout

    result = await buck.repl(
        input=b'r = ctx.cquery().eval("//:hello")\nlen(r)\n_ + 1\n',
    )
    assert result.stdout == "1\n2\n"


@buck_test()
async def test_repl_print(buck: Buck) -> None:
    result = await buck.repl("-e", 'print("hi")', "-e", 'ctx.output.print("out")')
    assert result.stdout == "hi\nout\n"


@buck_test()
async def test_repl_ensure(buck: Buck) -> None:
    # An ensured artifact is built and materialized after the input, and prints as its path.
    result = await buck.repl(
        "-e",
        'a = ctx.analysis(ctx.configured_targets("//:hello")).providers()[DefaultInfo].default_outputs[0]',
        "-e",
        "ctx.output.ensure(a)",
    )
    path = result.stdout.splitlines()[-1]
    assert path.startswith("buck-out/")
    assert (buck.cwd / path).read_text() == "hello\n"


@buck_test()
async def test_repl_error(buck: Buck) -> None:
    await expect_failure(
        buck.repl("-e", "nope"),
        exit_code=ExitCodeV2.USER_ERROR,
        stderr_regex="<repl:1>",
    )


@buck_test()
async def test_repl_continue_on_error(buck: Buck) -> None:
    # A failing input stops the inputs...
    failure = await expect_failure(
        buck.repl("-e", "nope", "-e", "1 + 1"),
        exit_code=ExitCodeV2.USER_ERROR,
    )
    assert failure.stdout == ""
    # ... unless `--continue-on-error` is given (the exit code is still 3).
    failure = await expect_failure(
        buck.repl("--continue-on-error", "-e", "nope", "-e", "1 + 1"),
        exit_code=ExitCodeV2.USER_ERROR,
    )
    assert failure.stdout == "2\n"


@buck_test()
async def test_repl_bxl_actions(buck: Buck) -> None:
    await expect_failure(
        buck.repl("-e", "ctx.bxl_actions()"),
        stderr_regex=r"\.bxl",
    )
    # The daemon is still there.
    result = await buck.repl("-e", "1 + 1")
    assert result.stdout == "2\n"


@buck_test()
async def test_repl_load(buck: Buck) -> None:
    result = await buck.repl(
        "-e", 'load("//pkg:helpers.bxl", "double")', "-e", "double(21)"
    )
    assert result.stdout == "42\n"


@buck_test()
async def test_repl_limits(buck: Buck) -> None:
    # A long value is cut.
    result = await buck.repl("-e", "list(range(200000))")
    assert len(result.stdout) < 70000
    assert "cut at 64 KiB" in result.stdout

    # A value nested too deeply to format is not formatted.
    result = await buck.repl(
        "-e", "x = []", "-e", "for i in range(100000): x = [x]", "-e", "x"
    )
    assert "nested too deeply" in result.stdout

    # The heap of the session is limited.
    await expect_failure(
        buck.repl("--max-heap-mb", "64", "-e", "y = [str(i) for i in range(5000000)]"),
        stderr_regex="--max-heap-mb",
    )

    # The daemon is still there.
    result = await buck.repl("-e", "1 + 1")
    assert result.stdout == "2\n"


@buck_test()
async def test_repl_no_stack_overflow(buck: Buck) -> None:
    # Inputs that would overflow the native stack (which aborts the daemon) fail instead.
    failure = await expect_failure(
        buck.repl(
            "--continue-on-error",
            "-e",
            'x = cmd_args("a")',
            "-e",
            'for i in range(100000): x = cmd_args(x, "b")',
            "-e",
            ":j x",
            "-e",
            "y = []",
            "-e",
            "y.append(y)",
            "-e",
            "ctx.output.print_json(y)",
            "-e",
            "c = cmd_args(y)",
            "-e",
            'z = ctx.lazy.unconfigured_target_node("//:hello")',
            "-e",
            "for i in range(1000): z = z.catch()",
            "-e",
            ":cq " + "deps(" * 5000 + "//:hello" + ")" * 5000,
        ),
        exit_code=ExitCodeV2.USER_ERROR,
    )
    assert "nested too deeply" in failure.stderr
    assert "Cycle detected" in failure.stderr
    assert "contains itself" in failure.stderr
    assert "Lazy operations nest too deeply" in failure.stderr
    assert "more than 500 levels" in failure.stderr

    # An int too large to format quickly is shown as its size.
    result = await buck.repl(
        "-e", "n = 1 << 1000", "-e", "for i in range(12): n = n * n", "-e", "[n]"
    )
    assert (
        "<value holding an int of 4096001 bits: too large to display>" in result.stdout
    )

    # The daemon is still there.
    result = await buck.repl("-e", "1 + 1")
    assert result.stdout == "2\n"


# Commands.


@buck_test()
async def test_repl_cwd(buck: Buck) -> None:
    # Patterns are relative to the session's directory.
    result = await buck.repl(input=b":cq :lib\nlen(_)\n", rel_cwd=Path("pkg"))
    lines = result.stdout.splitlines()
    assert "root//pkg:lib" in lines[0]
    assert lines[-1] == "1"


@buck_test()
async def test_repl_commands(buck: Buck) -> None:
    result = await buck.repl("-e", ":t 1")
    assert result.stdout == "int\n"

    result = await buck.repl("-e", ':j {"a": [1]}')
    assert json.loads(result.stdout) == {"a": [1]}

    result = await buck.repl("-e", ":doc ctx")
    assert "cquery" in result.stdout

    result = await buck.repl("-e", ":pv //:hello")
    assert "DefaultInfo" in result.stdout

    result = await buck.repl("-e", ":help")
    assert ":build" in result.stdout


@buck_test()
async def test_repl_type_of_callables(buck: Buck) -> None:
    # A native method bound to its object shows its signature, as a native function does.
    for expr in ["ctx.configured_targets", "ctx.cquery().deps", "len"]:
        result = await buck.repl("-e", f":t {expr}")
        assert result.stdout.startswith("def("), (expr, result.stdout)
        assert result.stdout.endswith('  # type() is "function"\n'), (
            expr,
            result.stdout,
        )
    result = await buck.repl("-e", ":t ctx.configured_targets")
    assert "target_platform: None | TargetLabel | str = ..." in result.stdout
    # A def: its default values are never shown.
    result = await buck.repl("-e", "def f(a, b = [1, 2]): return a", "-e", ":t f")
    assert result.stdout == (
        'def(a: typing.Any, b: typing.Any = ...) -> typing.Any  # type() is "function"\n'
    )


@buck_test()
async def test_repl_doc_rendered(buck: Buck) -> None:
    # Documentation (Markdown) is shown as plain text in scripts: no heading marks, code
    # fences or escapes; code is indented.
    result = await buck.repl("-e", ":doc ctx.configured_targets")
    lines = result.stdout.splitlines()
    assert lines[0] == "ctx.configured_targets", lines[:3]
    assert "    def ctx.configured_targets(" in lines
    assert "```" not in result.stdout
    assert "\\_" not in result.stdout
    assert not any(line.startswith("#") for line in lines)

    result = await buck.repl("-e", ":qdoc rdeps")
    assert result.stdout.startswith(
        "rdeps(universe: target expression, "
    ), result.stdout

    # With `--json`, the record holds the text as shown.
    result = await buck.repl("--json", "-e", ":doc len")
    [record] = _json_lines(result.stdout)
    assert record["stdout"].startswith("len\n\n    def len("), record["stdout"]


@buck_test()
async def test_repl_load_command(buck: Buck) -> None:
    result = await buck.repl(
        "-e", ":l //pkg:helpers.bxl", "-e", "double(2)", "-e", ":r", "-e", "double(3)"
    )
    assert result.stdout == "4\n6\n"
    assert "loaded //pkg:helpers.bxl: double, main" in result.stderr
    assert "reloaded //pkg:helpers.bxl" in result.stderr

    # `:reset` forgets every binding.
    await expect_failure(
        buck.repl("-e", ":l //pkg:helpers.bxl", "-e", ":reset", "-e", "double"),
        exit_code=ExitCodeV2.USER_ERROR,
        stderr_regex="double",
    )


# `:!` runs a POSIX shell command here.
@buck_test(skip_for_os=["windows"])
async def test_repl_reload_edited(buck: Buck) -> None:
    # `:reload` loads a module again once it was changed (here, by the session itself).
    (buck.cwd / "pkg" / "edit.bxl").write_text("def f(x):\n    return x * 2\n")
    result = await buck.repl(
        "-e",
        ":l //pkg:edit.bxl",
        "-e",
        "f(2)",
        "-e",
        ":!printf 'def f(x):\\n    return x * 3\\n' > pkg/edit.bxl",
        "-e",
        ":r",
        "-e",
        "f(2)",
    )
    assert result.stdout == "4\n6\n"
    assert "reloaded //pkg:edit.bxl" in result.stderr


@buck_test()
async def test_repl_bad_commands(buck: Buck) -> None:
    failure = await expect_failure(
        buck.repl("--continue-on-error", "-e", ":re", "-e", ":zz", "-e", "1 + 1"),
        exit_code=ExitCodeV2.USER_ERROR,
    )
    assert "ambiguous" in failure.stderr
    assert "unknown command" in failure.stderr
    assert failure.stdout == "2\n"


@buck_test()
async def test_repl_bxl_unchanged(buck: Buck) -> None:
    result = await buck.bxl("//pkg:helpers.bxl:main")
    assert result.stdout == "hello from main\n"


@buck_test()
async def test_repl_bxl_command(buck: Buck) -> None:
    result = await buck.repl("-e", ":bxl //pkg:helpers.bxl:main")
    assert result.stdout == "hello from main\n"

    failure = await expect_failure(
        buck.repl(
            "--continue-on-error", "-e", ":bxl //pkg:nope.bxl:main", "-e", "1 + 1"
        ),
        exit_code=ExitCodeV2.USER_ERROR,
    )
    assert failure.stdout == "2\n"


@buck_test()
async def test_repl_inspect(buck: Buck) -> None:
    result = await buck.repl("-e", ":ls")
    assert "root//:greet" in result.stdout
    assert "root//:hello" in result.stdout

    result = await buck.repl("-e", ":info :hello")
    assert "write_file" in result.stdout
    assert "TARGETS.fixture:12" in result.stdout

    result = await buck.repl("-e", ":qdoc rdeps")
    assert "rdeps" in result.stdout

    result = await buck.repl("-e", "my_value = 1", "-e", ":who my_*")
    assert "my_value" in result.stdout

    result = await buck.repl("-e", "1 + 1", "-e", ":hist")
    assert result.stdout == "2\n   1  1 + 1\n"


@buck_test()
async def test_repl_files(buck: Buck) -> None:
    (buck.cwd / "code.star").write_text("y = double(5) + 1\n")
    result = await buck.repl("pkg/helpers.bxl", "code.star", "-e", "y")
    assert result.stdout == "11\n"


# `:build` and `:run`.


@buck_test()
async def test_repl_build(buck: Buck) -> None:
    result = await buck.repl("-e", ":b //:hello", "-e", "type(_)")
    lines = result.stdout.splitlines()
    assert lines[0].startswith("root//:hello  buck-out/")
    path = lines[0].split("  ")[1]
    assert (buck.cwd / path).read_text() == "hello\n"
    assert lines[1] == '"dict"'

    # Build options apply to the builds of the session.
    result = await buck.repl("--prefer-local", "-e", ":b //:hello")
    assert "root//:hello" in result.stdout

    failure = await expect_failure(
        buck.repl("--continue-on-error", "-e", ":b //nope:x", "-e", "1 + 1"),
        exit_code=ExitCodeV2.USER_ERROR,
    )
    assert failure.stdout == "2\n"


# The program of `//:greet` is `echo`.
@buck_test(skip_for_os=["windows"])
async def test_repl_run(buck: Buck) -> None:
    result = await buck.repl("-e", ":run //:greet -- a b")
    assert "hello from greet a b" in result.stdout
    assert "[exited 0" in result.stderr

    result = await buck.repl("-e", ":run --print //:greet")
    assert result.stdout == "echo 'hello from greet'\n"


# Completion (`:__complete`).


@buck_test()
async def test_repl_complete(buck: Buck) -> None:
    answers = await _complete(
        buck,
        "ct",
        "ctx.cquery().de",
        ":b //:he",
        ":b //:hello[",
        "ctx.configured_targets(tar",
        ":cq dep",
        'ctx.cquery().eval("rde',
        'load("//pkg:hel',
        'load("//pkg:helpers.bxl", "',
        ":bxl //pkg:helpers.bxl:",
    )
    assert "ctx" in answers[0]
    assert answers[1] == ["deps("]
    assert "//:hello" in answers[2]
    assert answers[3] == ["//:hello[out]"]
    assert "target_platform=" in answers[4]
    assert answers[5] == ["deps("]
    assert answers[6] == ["rdeps("]
    assert answers[7] == ['//pkg:helpers.bxl"']
    assert sorted(answers[8]) == ['double"', 'main"']
    assert answers[9] == ["//pkg:helpers.bxl:main"]


@buck_test()
async def test_repl_complete_names(buck: Buck) -> None:
    answers = await _complete(
        buck,
        "ctx.c",
        "ctx.output.p",
        ":b //",
        ":b //:",
        'ctx.configured_targets("//pkg:l',
        "nope.x",
    )
    assert {"cquery(", "configured_targets(", "cell_root("} <= set(answers[0])
    assert {"print(", "print_json("} <= set(answers[1])
    assert {"//pkg/", "//pkg:", "//:", "//..."} <= set(answers[2])
    assert answers[3] == ["//:greet", "//:hello"]
    assert answers[4] == ['//pkg:lib"']
    assert answers[5] == []

    # Relative patterns complete in the session's directory.
    [answer] = await _complete(buck, ":b :", rel_cwd=Path("pkg"))
    assert answer == [":lib"]


@buck_test()
async def test_repl_complete_session(buck: Buck) -> None:
    # Completion sees what earlier inputs defined or loaded.
    result = await buck.repl(
        "-e",
        "s = struct(foo=1, far=2)",
        "-e",
        _complete_input("s.f"),
        "-e",
        ":l //pkg:helpers.bxl",
        "-e",
        _complete_input("dou"),
        "-e",
        "1 + 1",
        "-e",
        _complete_input("_"),
        "-e",
        _complete_input(":b //nope/"),
        "-e",
        "2 + 2",
        env=_COMPLETION_ENV,
    )
    lines = result.stdout.splitlines()
    assert len(lines) == 6, result.stdout
    assert _replacements(json.loads(lines[0])) == ["far", "foo"]
    assert _replacements(json.loads(lines[1])) == ["double("]
    assert lines[2] == "2"
    assert "_" in _replacements(json.loads(lines[3]))
    # A package that does not exist has no candidates (or fails): the session goes on.
    nope = json.loads(lines[4])
    assert nope["status"] == "error" or nope["candidates"] == [], nope
    assert lines[5] == "4"


@buck_test()
async def test_repl_complete_through_calls(buck: Buck) -> None:
    # The default values of these functions are huge: completion must not format them.
    result = await buck.repl(
        "-e",
        ":l //pkg:typed.bxl",
        "-e",
        ':__complete {"buf": "typed().up"}',
        "-e",
        ':__complete {"buf": "cq(ctx).de"}',
        "-e",
        ':__complete {"buf": "untyped().x"}',
        "-e",
        "1 + 1",
    )
    lines = result.stdout.splitlines()
    assert [c["replacement"] for c in json.loads(lines[0])["candidates"]] == ["upper("]
    assert [c["replacement"] for c in json.loads(lines[1])["candidates"]] == ["deps("]
    assert json.loads(lines[2])["candidates"] == []
    assert lines[3] == "2"


# `--json`.


@buck_test()
async def test_repl_json(buck: Buck) -> None:
    failure = await expect_failure(
        buck.repl("--json", "-e", "x = 1", "-e", "x + 1", "-e", "nope"),
        exit_code=ExitCodeV2.USER_ERROR,
    )
    records = _json_lines(failure.stdout)
    assert len(records) == 3
    assert records[0]["n"] == 1
    assert records[0]["ok"]
    assert "type" not in records[0]
    assert records[1]["type"] == "int"
    assert records[1]["text"] == "2"
    assert records[1]["json"] == 2
    assert not records[2]["ok"]
    assert records[2]["error"]["kind"] == "eval"
    assert "<repl:3>" in records[2]["error"]["message"]


@buck_test()
async def test_repl_json_output(buck: Buck) -> None:
    result = await buck.repl(
        "--json",
        "-e",
        'print("hi")',
        "-e",
        'ctx.output.stream("streamed")',
        "-e",
        ":bxl //pkg:helpers.bxl:main",
        "-e",
        ":b //:hello",
        "-e",
        ":run //:greet -- a",
        "-e",
        ":l //pkg:helpers.bxl",
        "-e",
        ":help quit",
    )
    records = _json_lines(result.stdout)
    assert records[0]["stdout"] == "hi\n"
    assert records[1]["stdout"] == "streamed\n"
    assert records[2]["stdout"] == "hello from main\n"
    [(label, [path])] = records[3]["json"].items()
    assert label == "root//:hello"
    assert (buck.cwd / path).read_text() == "hello\n"
    # `:run` is not run: its command line is given.
    assert records[4]["run"]["argv"] == ["echo", "hello from greet", "a"]
    assert records[4]["stdout"] == "echo 'hello from greet' a\n"
    assert records[5]["notices"] == [
        {"level": "info", "text": "loaded //pkg:helpers.bxl: double, main"}
    ]
    # Inputs the client handles alone have no number.
    assert records[6]["n"] is None
    assert ":quit" in records[6]["stdout"]
