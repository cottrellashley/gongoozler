from __future__ import annotations

import json

import pytest

import gongoozler as gz

from .conftest import Recording


def test_defaults_hold_when_nothing_is_given(recorded: Recording, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("GONGOOZLER_PARAMS", raising=False)
    p = gz.params(fast=10, slow=50.0, venue="SIM", hedge=False)
    assert (p.fast, p.slow, p.venue, p.hedge) == (10, 50.0, "SIM", False)
    assert recorded.of("params")[0]["values"] == {"fast": 10, "slow": 50.0, "venue": "SIM", "hedge": False}


def test_given_values_take_their_defaults_types(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GONGOOZLER_PARAMS", json.dumps({"fast": "20", "slow": 75, "hedge": "yes", "venue": 3}))
    p = gz.params(fast=10, slow=50.0, venue="SIM", hedge=False)
    assert (p.fast, p.slow, p.venue, p.hedge) == (20, 75.0, "3", True)


def test_an_undeclared_parameter_is_an_error(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GONGOOZLER_PARAMS", json.dumps({"fats": 20}))
    with pytest.raises(KeyError, match=r"unknown parameter.*fats.*declares fast"):
        gz.params(fast=10)


def test_a_value_that_cannot_take_the_type_is_an_error(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GONGOOZLER_PARAMS", json.dumps({"fast": "ten"}))
    with pytest.raises(TypeError, match="parameter fast: 'ten' is not a int"):
        gz.params(fast=10)
