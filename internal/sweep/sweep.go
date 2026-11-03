// Package sweep expands a sweep's grid into runs and ranks them by a statistic they kept
// (spec/run-file.md, "Sweeps").
package sweep

import (
	"encoding/json"
	"math"
	"sort"
	"strings"
)

// Grid is each swept parameter's values.
type Grid map[string][]any

// Points is every combination of the grid's values, in a stable order: parameters by name, the
// last varying fastest.
func (g Grid) Points() []map[string]any {
	names := make([]string, 0, len(g))
	for k := range g {
		names = append(names, k)
	}
	sort.Strings(names)
	out := []map[string]any{{}}
	for _, k := range names {
		var next []map[string]any
		for _, p := range out {
			for _, v := range g[k] {
				q := make(map[string]any, len(p)+1)
				for kk, vv := range p {
					q[kk] = vv
				}
				q[k] = v
				next = append(next, q)
			}
		}
		out = next
	}
	if len(names) == 0 {
		return nil
	}
	return out
}

// Values reads a swept parameter's values from the command line: a JSON array, or values
// separated by commas, each read as JSON when it is JSON and as a string when it is not.
func Values(s string) []any {
	var list []any
	if strings.HasPrefix(strings.TrimSpace(s), "[") && decode(s, &list) == nil {
		return list
	}
	for _, part := range strings.Split(s, ",") {
		list = append(list, Value(strings.TrimSpace(part)))
	}
	return list
}

// Value reads one parameter value: JSON when it is JSON (numbers stay exact), else a string.
func Value(s string) any {
	var v any
	if decode(s, &v) != nil {
		return s
	}
	return v
}

func decode(s string, v any) error {
	d := json.NewDecoder(strings.NewReader(s))
	d.UseNumber()
	if err := d.Decode(v); err != nil {
		return err
	}
	if d.More() {
		return errTrailing
	}
	return nil
}

type trailing struct{}

func (trailing) Error() string { return "data after the value" }

var errTrailing = trailing{}

// Stat reads a number at a dotted path in a run's statistics, such as "pnls.USD.PnL (total)".
func Stat(stats map[string]any, path string) (float64, bool) {
	var cur any = stats
	for _, k := range strings.Split(path, ".") {
		m, ok := cur.(map[string]any)
		if !ok {
			return 0, false
		}
		if cur, ok = m[k]; !ok {
			return 0, false
		}
	}
	switch v := cur.(type) {
	case float64:
		return v, !math.IsNaN(v) && !math.IsInf(v, 0)
	case json.Number:
		f, err := v.Float64()
		return f, err == nil && !math.IsNaN(f) && !math.IsInf(f, 0)
	}
	return 0, false
}

// Entry is one run of a sweep, as ranked.
type Entry struct {
	ID     string         `json:"id"`
	Params map[string]any `json:"params"`
	Status string         `json:"status"`
	Value  *float64       `json:"value,omitempty"`
}

// Rank orders entries by value, highest first, or lowest first when by starts with "-". Entries
// with no value come last, in their order.
func Rank(entries []Entry, by string) {
	asc := strings.HasPrefix(by, "-")
	sort.SliceStable(entries, func(i, j int) bool {
		a, b := entries[i].Value, entries[j].Value
		switch {
		case a == nil:
			return false
		case b == nil:
			return true
		case asc:
			return *a < *b
		}
		return *a > *b
	})
}

// Path is the statistic's path, without the "-" that orders lowest first.
func Path(by string) string { return strings.TrimPrefix(by, "-") }

// Label is a run's parameters in sorted order, as name=value pairs.
func Label(params map[string]any) string {
	names := make([]string, 0, len(params))
	for k := range params {
		names = append(names, k)
	}
	sort.Strings(names)
	parts := make([]string, len(names))
	for i, k := range names {
		b, _ := json.Marshal(params[k])
		v := string(b)
		if s, ok := params[k].(string); ok && !strings.ContainsAny(s, " =,") {
			v = s
		}
		parts[i] = k + "=" + v
	}
	return strings.Join(parts, " ")
}
