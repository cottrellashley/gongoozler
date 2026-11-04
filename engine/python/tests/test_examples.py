"""The examples run as written."""

import pathlib
import runpy

import pytest

EXAMPLES = sorted((pathlib.Path(__file__).parents[2] / "examples").glob("*.py"))


@pytest.mark.parametrize("path", EXAMPLES, ids=[p.name for p in EXAMPLES])
def test_example_runs(path, capsys):
    runpy.run_path(str(path), run_name="__main__")
    assert capsys.readouterr().out
