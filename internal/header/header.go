// Package header reads a run file's PEP 723 header: its `script` block, and in it the
// `[tool.gongoozler]` table that sizes the run (spec/run-file.md).
//
//	# /// script
//	# dependencies = ["gongoozler", "nautilus_trader==2.0.0rc8"]
//	# [tool.gongoozler]
//	# memory = "48GiB"
//	# cpus = 8
//	# timeout = "3h"
//	# placement = "auto"
//	# sweep = { fast = [10, 20], slow = [50, 100] }
//	# ///
package header

import (
	"errors"
	"fmt"
	"os"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"
)

// Settings are what [tool.gongoozler] says about a run.
type Settings struct {
	Memory    int64         // bytes; 0 when not declared
	CPUs      float64       // 0 when not declared
	Timeout   time.Duration // 0 when not declared
	Placement string        // "auto" (the default), "local" or "remote"
	Sweep     map[string][]any
}

// Header is a file's script block.
type Header struct {
	Found        bool
	Dependencies []string
	Settings     Settings
	Raw          map[string]any
}

var block = regexp.MustCompile(`(?m)^# /// (?P<type>[a-zA-Z0-9-]+)$\s(?P<content>(^#(| .*)$\s)+)^# ///$`)

// Read reads path's header. A file with no script block has an empty header (Found false).
func Read(path string) (Header, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return Header{}, err
	}
	return Parse(string(b))
}

// Parse reads a header from a file's text.
func Parse(text string) (Header, error) {
	var found []string
	for _, m := range block.FindAllStringSubmatch(text, -1) {
		if m[1] == "script" {
			found = append(found, m[2])
		}
	}
	if len(found) == 0 {
		return Header{Settings: Settings{Placement: "auto"}}, nil
	}
	if len(found) > 1 {
		return Header{}, errors.New("more than one script block (PEP 723 allows one)")
	}
	var lines []string
	for _, l := range strings.Split(strings.TrimRight(found[0], "\n"), "\n") {
		l = strings.TrimPrefix(l, "#")
		lines = append(lines, strings.TrimPrefix(l, " "))
	}
	raw, err := parseTOML(strings.Join(lines, "\n") + "\n")
	if err != nil {
		return Header{}, fmt.Errorf("the script block: %w", err)
	}
	h := Header{Found: true, Raw: raw, Settings: Settings{Placement: "auto"}}
	if deps, ok := raw["dependencies"].([]any); ok {
		for _, d := range deps {
			if s, ok := d.(string); ok {
				h.Dependencies = append(h.Dependencies, s)
			}
		}
	}
	tool, _ := raw["tool"].(map[string]any)
	g, _ := tool["gongoozler"].(map[string]any)
	if g == nil {
		return h, nil
	}
	known := map[string]bool{"memory": true, "cpus": true, "timeout": true, "placement": true, "sweep": true}
	var unknown []string
	for k := range g {
		if !known[k] {
			unknown = append(unknown, k)
		}
	}
	if len(unknown) > 0 {
		sort.Strings(unknown)
		return Header{}, fmt.Errorf("[tool.gongoozler] has no setting %s (it has memory, cpus, timeout, placement and sweep)", strings.Join(unknown, ", "))
	}
	s := &h.Settings
	if v, ok := g["memory"]; ok {
		if s.Memory, err = Bytes(v); err != nil {
			return Header{}, fmt.Errorf("[tool.gongoozler] memory: %w", err)
		}
	}
	if v, ok := g["cpus"]; ok {
		switch n := v.(type) {
		case int64:
			s.CPUs = float64(n)
		case float64:
			s.CPUs = n
		default:
			return Header{}, fmt.Errorf("[tool.gongoozler] cpus: %v is not a number", v)
		}
		if s.CPUs <= 0 {
			return Header{}, errors.New("[tool.gongoozler] cpus must be positive")
		}
	}
	if v, ok := g["timeout"]; ok {
		if s.Timeout, err = Duration(v); err != nil {
			return Header{}, fmt.Errorf("[tool.gongoozler] timeout: %w", err)
		}
	}
	if v, ok := g["placement"]; ok {
		p, _ := v.(string)
		if p != "auto" && p != "local" && p != "remote" {
			return Header{}, fmt.Errorf("[tool.gongoozler] placement is auto, local or remote, not %v", v)
		}
		s.Placement = p
	}
	if v, ok := g["sweep"]; ok {
		t, ok := v.(map[string]any)
		if !ok {
			return Header{}, errors.New("[tool.gongoozler] sweep is a table of parameter names and lists of values")
		}
		s.Sweep = map[string][]any{}
		for k, vals := range t {
			list, ok := vals.([]any)
			if !ok || len(list) == 0 {
				return Header{}, fmt.Errorf("[tool.gongoozler] sweep.%s is not a non-empty list", k)
			}
			s.Sweep[k] = list
		}
	}
	return h, nil
}

var units = map[string]float64{
	"": 1, "b": 1,
	"k": 1e3, "kb": 1e3, "m": 1e6, "mb": 1e6, "g": 1e9, "gb": 1e9, "t": 1e12, "tb": 1e12, "p": 1e15, "pb": 1e15,
	"ki": 1 << 10, "kib": 1 << 10, "mi": 1 << 20, "mib": 1 << 20, "gi": 1 << 30, "gib": 1 << 30, "ti": 1 << 40, "tib": 1 << 40,
	"pi": 1 << 50, "pib": 1 << 50,
}

var sizeRe = regexp.MustCompile(`^\s*([0-9]*\.?[0-9]+)\s*([A-Za-z]*)\s*$`)

// Bytes reads a size: a number of bytes, or text such as "48GiB", "512 MiB", "2G" or "1.5GB".
func Bytes(v any) (int64, error) {
	switch n := v.(type) {
	case int64:
		if n <= 0 {
			return 0, errors.New("must be positive")
		}
		return n, nil
	case float64:
		if n <= 0 {
			return 0, errors.New("must be positive")
		}
		return int64(n), nil
	case string:
		m := sizeRe.FindStringSubmatch(n)
		if m == nil {
			return 0, fmt.Errorf("%q is not a size such as 48GiB", n)
		}
		f, _ := strconv.ParseFloat(m[1], 64)
		mult, ok := units[strings.ToLower(m[2])]
		if !ok || f <= 0 {
			return 0, fmt.Errorf("%q is not a size such as 48GiB", n)
		}
		return int64(f * mult), nil
	}
	return 0, fmt.Errorf("%v is not a size", v)
}

// Duration reads a time limit: seconds, or text such as "3h", "90m" or "1h30m".
func Duration(v any) (time.Duration, error) {
	switch n := v.(type) {
	case int64:
		if n <= 0 {
			return 0, errors.New("must be positive")
		}
		return time.Duration(n) * time.Second, nil
	case float64:
		if n <= 0 {
			return 0, errors.New("must be positive")
		}
		return time.Duration(n * float64(time.Second)), nil
	case string:
		d, err := time.ParseDuration(n)
		if err != nil || d <= 0 {
			return 0, fmt.Errorf("%q is not a duration such as 3h or 90m", n)
		}
		return d, nil
	}
	return 0, fmt.Errorf("%v is not a duration", v)
}
