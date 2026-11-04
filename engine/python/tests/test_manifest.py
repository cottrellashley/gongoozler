"""The run's own records from Python (spec, change 7): P12. A configuration built in code reruns
to the same output under the same manifest; a change is attributed; definitions declare what
they model, and a run needing more is refused unless the approximation is declared."""

from __future__ import annotations

import pytest

import fi_engine as fi


def run(bonds, n: int = 5):
    ws, rec, _ = bonds
    book = fi.Strategy(fi.Decision.top_n("marks.signal", n), fill=fi.Fill.research(2.0), journal=True)
    r = ws.run(book, rec)
    return r, ws.manifest(r, book, rec)


def test_p12_a_rerun_from_code_reproduces_its_output_under_the_same_manifest(bonds):
    r1, m1 = run(bonds)
    r2, m2 = run(bonds)
    assert m1 == m2 and m1.fingerprint == m2.fingerprint and m1.diff(m2) == []
    assert r1.journal().equals(r2.journal()), "a retry duplicates no events"
    d = m1.as_dict()
    assert d["mode"] == "policy backtest"
    assert {s["name"] for s in d["series"]} == {"marks"}
    assert d["fingerprint"] == f"{m1.fingerprint:016x}"


def test_p12_a_changed_policy_is_an_attributable_delta(bonds):
    _, m1 = run(bonds, 5)
    _, m2 = run(bonds, 6)
    delta = m1.diff(m2)
    assert any(x.startswith("policies:") for x in delta) and any(x.startswith("output:") for x in delta)
    assert not any(x.startswith("series") or x.startswith("model") for x in delta)


def test_definitions_declare_what_they_model(bonds):
    ws, _, _ = bonds
    bonds_ = ws.supplied_bonds("marks")
    matrix = bonds_.support_matrix()
    assert {"pik", "writedown", "coupon"} <= set(matrix.filter(matrix["kind"] == "supplied_bond")["feature"])
    with pytest.raises(fi.InvalidError, match="does not model floating coupon floor"):
        ws.records(bonds_, ws.calendar("marks"), requires={"supplied_bond": ["floating coupon floor"]})
    rec = ws.records(bonds_, ws.calendar("marks"), requires={"supplied_bond": ["writedown", "floating coupon floor"]},
                     approximate={"supplied_bond": ["floating coupon floor"]})
    assert rec.approximations == ["supplied_bond does not model floating coupon floor: approximated, as declared"]
    book = fi.Strategy(fi.Decision.top_n("marks.signal", 5))
    r = ws.run(book, rec)
    assert ws.manifest(r, book, rec).as_dict()["approximations"] == rec.approximations
