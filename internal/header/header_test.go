package header

import (
	"math"
	"strings"
	"testing"
	"time"
)

const file = `# /// script
# requires-python = ">=3.13"
# dependencies = [
#     "gongoozler",            # a comment
#     "nautilus_trader==2.0.0rc8",
# ]
# [[tool.uv.index]]
# url = "https://packages.firm.example/simple"
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# [tool.uv.sources]
# gongoozler = { path = "../python", editable = true }
# [tool.gongoozler]
# memory = "48GiB"
# cpus = 8
# timeout = "1h30m"
# sweep = { fast = [10, 20], slow = [50, 100, 200], name = ['a', "b"] }
# ///
import gongoozler
`

func TestAFullHeaderReadsBack(t *testing.T) {
	h, err := Parse(file)
	if err != nil {
		t.Fatal(err)
	}
	if !h.Found || strings.Join(h.Dependencies, ",") != "gongoozler,nautilus_trader==2.0.0rc8" {
		t.Fatalf("dependencies %v", h.Dependencies)
	}
	s := h.Settings
	if s.Memory != 48<<30 || s.CPUs != 8 || s.Timeout != 90*time.Minute || s.Placement != "auto" {
		t.Fatalf("settings %+v", s)
	}
	if len(s.Sweep["slow"]) != 3 || s.Sweep["fast"][1] != int64(20) || s.Sweep["name"][0] != "a" {
		t.Fatalf("sweep %v", s.Sweep)
	}
	idx := h.Raw["tool"].(map[string]any)["uv"].(map[string]any)["index"].([]map[string]any)
	if idx[0]["url"] != "https://packages.firm.example/simple" {
		t.Fatalf("index %v", idx)
	}
	src := h.Raw["tool"].(map[string]any)["uv"].(map[string]any)["sources"].(map[string]any)["gongoozler"].(map[string]any)
	if src["editable"] != true {
		t.Fatalf("sources %v", src)
	}
}

func TestAFileWithNoHeaderHasDefaults(t *testing.T) {
	h, err := Parse("print('hi')\n")
	if err != nil || h.Found || h.Settings.Placement != "auto" || h.Settings.Memory != 0 {
		t.Fatalf("%+v %v", h, err)
	}
}

func TestMistakesAreNamed(t *testing.T) {
	for text, want := range map[string]string{
		"# /// script\n# dependencies = []\n# [tool.gongoozler]\n# memroy = \"1G\"\n# ///\n":       "no setting memroy",
		"# /// script\n# dependencies = []\n# [tool.gongoozler]\n# memory = \"lots\"\n# ///\n":     `"lots" is not a size`,
		"# /// script\n# dependencies = []\n# [tool.gongoozler]\n# placement = \"cloud\"\n# ///\n": "auto, local or remote",
		"# /// script\n# dependencies = [\n# ///\n":                                                "the script block",
		"# /// script\n# a = 1\n# a = 2\n# ///\n":                                                  "defined twice",
		"# /// script\n# x = \"\n# ///\n":                                                          "unterminated string",
	} {
		_, err := Parse(text)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%q: got %v, want %q", text, err, want)
		}
	}
}

func TestSizesAndDurations(t *testing.T) {
	for in, want := range map[any]int64{"48GiB": 48 << 30, "512 MiB": 512 << 20, "2G": 2e9, "1.5GB": 1.5e9, int64(1024): 1024, "100k": 1e5} {
		got, err := Bytes(in)
		if err != nil || got != want {
			t.Errorf("Bytes(%v) = %d, %v; want %d", in, got, err, want)
		}
	}
	if d, err := Duration(int64(90)); err != nil || d != 90*time.Second {
		t.Fatal(d, err)
	}
	if _, err := Duration("soon"); err == nil {
		t.Fatal("a bad duration was accepted")
	}
}

func TestTOMLValues(t *testing.T) {
	v, err := parseTOML(`
a = "esc\"apedé\n"
b = 'lit\eral'
c = """
multi
line"""
d = 1_000
e = -3.5e2
f = inf
g = nan
h = [1, [2, 3], { x = 1 },]
i = 2026-11-01T00:00:00Z
j.k = true
"quoted key" = 0x1F
`)
	if err != nil {
		t.Fatal(err)
	}
	if v["a"] != "esc\"apedé\n" || v["b"] != `lit\eral` || v["c"] != "multi\nline" || v["d"] != int64(1000) || v["e"] != -350.0 {
		t.Fatalf("%v", v)
	}
	if !math.IsInf(v["f"].(float64), 1) || !math.IsNaN(v["g"].(float64)) {
		t.Fatalf("inf and nan: %v %v", v["f"], v["g"])
	}
	if h := v["h"].([]any); len(h) != 3 || h[1].([]any)[1] != int64(3) || h[2].(map[string]any)["x"] != int64(1) {
		t.Fatalf("arrays: %v", v["h"])
	}
	if v["i"] != "2026-11-01T00:00:00Z" || v["j"].(map[string]any)["k"] != true || v["quoted key"] != int64(31) {
		t.Fatalf("%v", v)
	}
}

func FuzzParse(f *testing.F) {
	f.Add(file)
	f.Add("# /// script\n# a = [1, {b = 'c'}]\n# ///\n")
	f.Fuzz(func(t *testing.T, text string) {
		_, _ = Parse(text) // must never panic
	})
}
