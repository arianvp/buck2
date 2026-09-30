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
from typing import Any, Dict, List

from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.api.buck_result import ExitCodeV2
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test


def _json_lines(stdout: str) -> List[Dict[str, Any]]:
    return [json.loads(line) for line in stdout.splitlines()]


async def _complete(buck: Buck, *bufs: str) -> List[List[str]]:
    """The replacements `:__complete` offers for each buffer (completed at its end)."""
    inputs = [":__complete " + json.dumps({"buf": buf}) for buf in bufs]
    args = [arg for i in inputs for arg in ("-e", i)]
    result = await buck.repl(*args)
    answers = _json_lines(result.stdout)
    assert len(answers) == len(bufs), result.stdout
    for answer in answers:
        assert answer["status"] == "ok", answer
    return [[c["replacement"] for c in answer["candidates"]] for answer in answers]


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
    assert "truncated" in result.stdout

    # A value nested too deeply to format is not formatted.
    result = await buck.repl(
        "-e", "x = []", "-e", "for i in range(100000): x = [x]", "-e", "x"
    )
    assert "nested too deeply" in result.stdout

    # The heap of the session is limited.
    await expect_failure(
        buck.repl(
            "--max-heap-mb", "64", "-e", "y = [str(i) for i in range(5000000)]"
        ),
        stderr_regex="--max-heap-mb",
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


@buck_test()
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
    assert records[5]["notices"] == [
        {"level": "info", "text": "loaded //pkg:helpers.bxl: double, main"}
    ]
    # Inputs the client handles alone have no number.
    assert records[6]["n"] is None
    assert ":quit" in records[6]["stdout"]
