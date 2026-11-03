package version

import (
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"testing"
)

func TestThePythonLibraryCarriesTheSameVersion(t *testing.T) {
	_, here, _, _ := runtime.Caller(0)
	root := filepath.Join(filepath.Dir(here), "..", "..", "python")
	for file, pattern := range map[string]string{
		"pyproject.toml":             `(?m)^version = "([^"]+)"$`,
		"src/gongoozler/__init__.py": `(?m)^__version__ = "([^"]+)"$`,
	} {
		b, err := os.ReadFile(filepath.Join(root, file))
		if err != nil {
			t.Fatal(err)
		}
		m := regexp.MustCompile(pattern).FindSubmatch(b)
		if m == nil {
			t.Fatalf("no version in %s", file)
		}
		if string(m[1]) != Version {
			t.Errorf("%s says %s; the CLI is %s", file, m[1], Version)
		}
	}
}
