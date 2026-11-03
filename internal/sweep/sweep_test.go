package sweep

import (
	"encoding/json"
	"reflect"
	"testing"
)

func TestPointsAreEveryCombinationInAStableOrder(t *testing.T) {
	g := Grid{"slow": {50, 100}, "fast": {10, 20, 30}}
	var got []string
	for _, p := range g.Points() {
		got = append(got, Label(p))
	}
	want := []string{
		"fast=10 slow=50", "fast=10 slow=100", "fast=20 slow=50", "fast=20 slow=100", "fast=30 slow=50", "fast=30 slow=100",
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v", got)
	}
	if (Grid{}).Points() != nil {
		t.Fatal("an empty grid has no points")
	}
}

func TestValuesAreReadAsJSONWhereTheyCanBe(t *testing.T) {
	cases := map[string][]any{
		"1,2,3":            {json.Number("1"), json.Number("2"), json.Number("3")},
		`[1, "a,b", true]`: {json.Number("1"), "a,b", true},
		"ema, sma":         {"ema", "sma"},
		"0.5":              {json.Number("0.5")},
	}
	for in, want := range cases {
		if got := Values(in); !reflect.DeepEqual(got, want) {
			t.Errorf("Values(%q) = %#v", in, got)
		}
	}
	if v := Value("1 2"); v != "1 2" {
		t.Fatalf("trailing data is a string: %#v", v)
	}
}

func TestRunsRankByAStatisticHighestFirstOrLowestWithAMinus(t *testing.T) {
	var stats map[string]any
	if err := json.Unmarshal([]byte(`{"pnls": {"USD": {"PnL (total)": 12.5, "Max Loser": null}}}`), &stats); err != nil {
		t.Fatal(err)
	}
	if v, ok := Stat(stats, "pnls.USD.PnL (total)"); !ok || v != 12.5 {
		t.Fatalf("got %v %v", v, ok)
	}
	if _, ok := Stat(stats, "pnls.USD.Max Loser"); ok {
		t.Fatal("null is no value")
	}
	if _, ok := Stat(stats, "pnls.EUR.PnL (total)"); ok {
		t.Fatal("a missing path is no value")
	}
	f := func(v float64) *float64 { return &v }
	entries := []Entry{{ID: "a", Value: f(1)}, {ID: "b"}, {ID: "c", Value: f(3)}, {ID: "d", Value: f(2)}}
	Rank(entries, "pnl")
	if ids := []string{entries[0].ID, entries[1].ID, entries[2].ID, entries[3].ID}; !reflect.DeepEqual(ids, []string{"c", "d", "a", "b"}) {
		t.Fatalf("descending: %v", ids)
	}
	Rank(entries, "-pnl")
	if ids := []string{entries[0].ID, entries[1].ID, entries[2].ID, entries[3].ID}; !reflect.DeepEqual(ids, []string{"a", "d", "c", "b"}) {
		t.Fatalf("ascending: %v", ids)
	}
	if Path("-pnls.USD") != "pnls.USD" {
		t.Fatal("path")
	}
}
