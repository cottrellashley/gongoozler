package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/cottrellashley/gongoozler/internal/procmem"
	"github.com/cottrellashley/gongoozler/internal/runs"
)

var bin string

func TestMain(m *testing.M) {
	dir, err := os.MkdirTemp("", "gongoozler-bin-")
	if err != nil {
		panic(err)
	}
	bin = filepath.Join(dir, "gongoozler")
	// The CLI under test is built with the race detector where it can be (it needs cgo).
	if _, err := exec.Command("go", "build", "-race", "-o", bin, ".").CombinedOutput(); err != nil {
		if out, err := exec.Command("go", "build", "-o", bin, ".").CombinedOutput(); err != nil {
			panic(string(out))
		}
	}
	code := m.Run()
	os.RemoveAll(dir)
	os.Exit(code)
}

func repo() string {
	_, here, _, _ := runtime.Caller(0)
	return filepath.Join(filepath.Dir(here), "..", "..")
}

func pythonPath() string { return "PYTHONPATH=" + filepath.Join(repo(), "python", "src") }

func write(t *testing.T, name, code string) string {
	t.Helper()
	p := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(p, []byte(code), 0o600); err != nil {
		t.Fatal(err)
	}
	return p
}

// syncBuffer is a buffer a child process writes while the test reads it.
type syncBuffer struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (s *syncBuffer) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.Write(p)
}
func (s *syncBuffer) String() string { s.mu.Lock(); defer s.mu.Unlock(); return s.b.String() }

// proc is a gongoozler process started in the background.
type proc struct {
	cmd         *exec.Cmd
	out, errOut *syncBuffer
	done        chan struct{}
	code        int
}

func start(t *testing.T, state string, env []string, args ...string) *proc {
	t.Helper()
	p := &proc{cmd: exec.Command(bin, args...), out: &syncBuffer{}, errOut: &syncBuffer{}, done: make(chan struct{})}
	p.cmd.Env = append(os.Environ(), append([]string{"GONGOOZLER_STATE=" + state}, env...)...)
	p.cmd.Stdout, p.cmd.Stderr = p.out, p.errOut
	if err := p.cmd.Start(); err != nil {
		t.Fatal(err)
	}
	go func() {
		err := p.cmd.Wait()
		if ee, ok := err.(*exec.ExitError); ok {
			p.code = ee.ExitCode()
		}
		close(p.done)
	}()
	t.Cleanup(func() {
		select {
		case <-p.done:
		default:
			p.cmd.Process.Signal(syscall.SIGTERM)
			select {
			case <-p.done:
			case <-time.After(10 * time.Second):
				p.cmd.Process.Kill()
				<-p.done
			}
		}
	})
	return p
}

func (p *proc) wait(t *testing.T, timeout time.Duration) int {
	t.Helper()
	select {
	case <-p.done:
		return p.code
	case <-time.After(timeout):
		t.Fatalf("still running after %v:\n%s", timeout, p.errOut.String())
	}
	return -1
}

// waitFor waits until the process has printed want on stderr.
func (p *proc) waitFor(t *testing.T, want string) {
	t.Helper()
	deadline := time.Now().Add(20 * time.Second)
	for !strings.Contains(p.errOut.String(), want) {
		if time.Now().After(deadline) {
			t.Fatalf("no %q in:\n%s", want, p.errOut.String())
		}
		select {
		case <-p.done:
			if !strings.Contains(p.errOut.String(), want) {
				t.Fatalf("ended (exit %d) with no %q in:\n%s", p.code, want, p.errOut.String())
			}
		case <-time.After(20 * time.Millisecond):
		}
	}
}

func gz(t *testing.T, state string, env []string, args ...string) (string, string, int) {
	t.Helper()
	p := start(t, state, env, args...)
	code := p.wait(t, 120*time.Second)
	return p.out.String(), p.errOut.String(), code
}

func decodeLines(t *testing.T, s string) []map[string]any {
	t.Helper()
	var out []map[string]any
	sc := bufio.NewScanner(strings.NewReader(s))
	sc.Buffer(make([]byte, 0, 1<<16), 1<<22)
	for sc.Scan() {
		var e map[string]any
		if err := json.Unmarshal(sc.Bytes(), &e); err != nil {
			t.Fatalf("not an event: %q", sc.Text())
		}
		out = append(out, e)
	}
	return out
}

func lsJSON(t *testing.T, state string) []runs.Meta {
	t.Helper()
	out, stderr, code := gz(t, state, nil, "ls", "--json")
	if code != 0 {
		t.Fatalf("ls: %s", stderr)
	}
	var all []runs.Meta
	if err := json.Unmarshal([]byte(out), &all); err != nil {
		t.Fatalf("ls --json: %v\n%s", err, out)
	}
	return all
}

var runID = regexp.MustCompile(`\b\d{8}T\d{6}Z-[0-9a-f]{8}\b`)

func needUv(t *testing.T) {
	if _, err := exec.LookPath("uv"); err != nil {
		t.Skip("uv is not installed")
	}
}

// --------------------------------------------------------------------------------- run

func TestUvRunsAPep723FileAndTheCLIReportsItAsJSON(t *testing.T) {
	needUv(t)
	src, _ := filepath.Abs(filepath.Join(repo(), "python"))
	file := write(t, "idea.py", `# /// script
# requires-python = ">=3.12"
# dependencies = ["gongoozler"]
# [tool.uv.sources]
# gongoozler = { path = "`+src+`", editable = true }
# ///
import gongoozler as gz
p = gz.params(n=3)
with gz.stage("work"):
    for i in range(p.n):
        gz.progress("steps", i + 1, p.n)
print("done", p.n)
`)
	out, stderr, code := gz(t, t.TempDir(), nil, "run", "--json", "--param", "n=4", file)
	if code != 0 {
		t.Fatalf("exit %d: %s", code, stderr)
	}
	var kinds []string
	var last map[string]any
	for _, e := range decodeLines(t, out) {
		if k := e["kind"].(string); k != "resource" {
			kinds = append(kinds, k)
		}
		last = e
	}
	got := strings.Join(kinds, " ")
	for _, want := range []string{"run.start", "hello", "params", "stage", "progress", "output", "run.end"} {
		if !strings.Contains(got, want) {
			t.Fatalf("no %s in %s", want, got)
		}
	}
	if last["kind"] != "run.end" || last["status"] != "ok" {
		t.Fatalf("last event %v", last)
	}
	if _, err := os.Stat(file + ".lock"); err != nil {
		t.Fatalf("the first run did not lock the file: %v\n%s", err, stderr)
	}
	if !strings.Contains(stderr, "locked idea.py.lock on its first run") {
		t.Fatalf("the lock was not announced:\n%s", stderr)
	}
}

func TestTheFirstRunLocksTheFileAndTheRunRecordsTheLock(t *testing.T) {
	needUv(t)
	state := t.TempDir()
	file := write(t, "idea.py", "# /// script\n# requires-python = \">=3.12\"\n# dependencies = []\n# ///\nprint('hi')\n")
	if _, stderr, code := gz(t, state, nil, "run", "--plain", file); code != 0 {
		t.Fatalf("exit %d: %s", code, stderr)
	}
	lock, err := sha256File(file + ".lock")
	if err != nil {
		t.Fatal(err)
	}
	all := lsJSON(t, state)
	if len(all) != 1 || all[0].Lock != lock || all[0].Status != "ok" {
		t.Fatalf("recorded %+v, lock %s", all, lock)
	}
	if _, err := os.Stat(filepath.Join(runs.Dir(state, all[0].ID), "source", "idea.py.lock")); err != nil {
		t.Fatal("the lock was not kept with the run")
	}
	// A file with no script block has nothing to lock, and --no-lock locks nothing.
	bare := write(t, "bare.py", "print('hi')\n")
	other := write(t, "other.py", "# /// script\n# dependencies = []\n# ///\nprint('hi')\n")
	gz(t, state, nil, "run", "--plain", bare)
	gz(t, state, nil, "run", "--plain", "--no-lock", other)
	for _, f := range []string{bare, other} {
		if _, err := os.Stat(f + ".lock"); err == nil {
			t.Fatalf("%s was locked", filepath.Base(f))
		}
	}
}

func TestPlainOutputIsLinesWithNoTerminalCodes(t *testing.T) {
	file := write(t, "idea.py", "import gongoozler as gz\nwith gz.stage('load'):\n    gz.log('hi')\n")
	_, stderr, code := gz(t, t.TempDir(), []string{pythonPath()}, "run", "--launcher", "python3", file)
	if code != 0 {
		t.Fatalf("exit %d: %s", code, stderr)
	}
	if strings.Contains(stderr, "\x1b[") {
		t.Fatal("terminal codes in plain output")
	}
	for _, want := range []string{"▸ load", "[info] hi", "✓ load", " ok in ", "events: "} {
		if !strings.Contains(stderr, want) {
			t.Fatalf("no %q in:\n%s", want, stderr)
		}
	}
}

func TestAnInterruptReachesTheRunAndIsReported(t *testing.T) {
	file := write(t, "idea.py", "import time, gongoozler as gz\nwith gz.stage('wait'):\n    time.sleep(30)\n")
	p := start(t, t.TempDir(), []string{pythonPath()}, "run", "--launcher", "python3", file)
	p.waitFor(t, "▸ wait")
	p.cmd.Process.Signal(syscall.SIGINT)
	if code := p.wait(t, 20*time.Second); code != 130 {
		t.Fatalf("exit %d, want 130:\n%s", code, p.errOut.String())
	}
	if !strings.Contains(p.errOut.String(), " interrupted in ") {
		t.Fatalf("not reported as interrupted:\n%s", p.errOut.String())
	}
}

// Through uv, as a real run goes: one interrupt reaches the file once (uv forwards it; the
// supervisor signals uv alone).
func TestThroughUvAnInterruptReachesTheFileOnce(t *testing.T) {
	needUv(t)
	dir := t.TempDir()
	got, ready := filepath.Join(dir, "got"), filepath.Join(dir, "ready")
	file := write(t, "idea.py", fmt.Sprintf(`# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
import signal, sys, time
got = []
signal.signal(signal.SIGINT, lambda n, f: got.append(n))
open(%q, "w").write("1")
t = time.time()
while time.time() - t < 30 and not got:
    time.sleep(0.01)
time.sleep(0.5)  # time for a second delivery, if there were one
open(%q, "w").write(str(len(got)))
sys.exit(130)
`, ready, got))
	p := start(t, t.TempDir(), nil, "run", "--plain", "--no-lock", file)
	for deadline := time.Now().Add(60 * time.Second); ; time.Sleep(10 * time.Millisecond) {
		if _, err := os.Stat(ready); err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("the file never started:\n%s", p.errOut.String())
		}
	}
	p.cmd.Process.Signal(syscall.SIGINT)
	if code := p.wait(t, 30*time.Second); code != 130 {
		t.Fatalf("exit %d:\n%s", code, p.errOut.String())
	}
	if b, _ := os.ReadFile(got); string(b) != "1" {
		t.Fatalf("the file got %q interrupts, want 1", b)
	}
}

// --------------------------------------------------------------------------------- supervisor death

func TestARunWhoseCLIIsKilledOutrightDiesWithinASecondAndIsListedLost(t *testing.T) {
	state, dir := t.TempDir(), t.TempDir()
	pidfile := filepath.Join(dir, "pid")
	ignore := ""
	if procmem.ScopesAvailable() {
		ignore = "signal.signal(signal.SIGTERM, signal.SIG_IGN)  # its cgroup is killed whatever it ignores"
	}
	file := write(t, "idea.py", fmt.Sprintf("import os, signal, time\n%s\nopen(%q, 'w').write(str(os.getpid()))\ntime.sleep(60)\n", ignore, pidfile))
	p := start(t, state, nil, "run", "--plain", "--launcher", "python3", file)
	var pid int
	for deadline := time.Now().Add(20 * time.Second); pid == 0; time.Sleep(10 * time.Millisecond) {
		b, _ := os.ReadFile(pidfile)
		pid, _ = strconv.Atoi(string(b))
		if time.Now().After(deadline) {
			t.Fatalf("the run never started:\n%s", p.errOut.String())
		}
	}
	p.cmd.Process.Kill()
	p.wait(t, 10*time.Second)
	killed := time.Now()
	for {
		st, err := procmem.Stat("/proc", pid)
		if err != nil || st.State == 'Z' {
			break
		}
		if time.Since(killed) > time.Second {
			syscall.Kill(pid, syscall.SIGKILL)
			t.Fatal("the run outlived its CLI by more than a second")
		}
		time.Sleep(5 * time.Millisecond)
	}
	all := lsJSON(t, state)
	if len(all) != 1 || all[0].Status != "lost" || all[0].Note == "" {
		t.Fatalf("listed %+v, want lost", all)
	}
	m, _ := runs.ReadMeta(state, all[0].ID)
	if m.Status != "lost" || m.Ended.IsZero() {
		t.Fatalf("its meta says %s, ended %v", m.Status, m.Ended)
	}
}

// --------------------------------------------------------------------------------- ls, events, attach, kill

func TestRunsAreListedAndTheirEventsFoundByPrefix(t *testing.T) {
	state := t.TempDir()
	file := write(t, "idea.py", "print('x')\n")
	if _, stderr, code := gz(t, state, nil, "run", "--launcher", "python3", file); code != 0 {
		t.Fatal(stderr)
	}
	ls, _, _ := gz(t, state, nil, "ls")
	lines := strings.Split(strings.TrimSpace(ls), "\n")
	if len(lines) != 2 || !strings.Contains(lines[1], " ok ") || !strings.Contains(lines[1], "idea.py") || !strings.Contains(lines[1], "local") {
		t.Fatalf("ls:\n%s", ls)
	}
	id := strings.Fields(lines[1])[0]
	out, _, code := gz(t, state, nil, "events", id[:12])
	if code != 0 || len(decodeLines(t, out)) < 3 {
		t.Fatalf("events for %s: %q", id[:12], out)
	}
	if _, stderr, code := gz(t, state, nil, "events", "nope"); code == 0 || !strings.Contains(stderr, `no run "nope"`) {
		t.Fatalf("unknown run: %d %s", code, stderr)
	}
}

const slowFile = `import time, gongoozler as gz
with gz.stage("work"):
    for i in range(30):
        gz.progress("steps", i + 1, 30)
        time.sleep(0.05)
print("finished")
`

func TestFollowingAndAttachingSeeARunThroughToItsEnd(t *testing.T) {
	state := t.TempDir()
	file := write(t, "idea.py", slowFile)
	p := start(t, state, []string{pythonPath()}, "run", "--plain", "--launcher", "python3", file)
	p.waitFor(t, "▸ work")
	id := runID.FindString(p.errOut.String())
	follow := start(t, state, nil, "events", "--follow", id)
	attach := start(t, state, nil, "attach", "--plain", id)
	if code := p.wait(t, 30*time.Second); code != 0 {
		t.Fatalf("run: exit %d", code)
	}
	if code := follow.wait(t, 10*time.Second); code != 0 {
		t.Fatalf("events --follow: exit %d: %s", code, follow.errOut.String())
	}
	evs := decodeLines(t, follow.out.String())
	if len(evs) == 0 || evs[0]["kind"] != "run.start" || evs[len(evs)-1]["kind"] != "run.end" {
		t.Fatalf("followed %d events, from %v to %v", len(evs), evs[0]["kind"], evs[len(evs)-1]["kind"])
	}
	if code := attach.wait(t, 10*time.Second); code != 0 {
		t.Fatalf("attach: exit %d: %s", code, attach.errOut.String())
	}
	for _, want := range []string{"✓ work", " ok in "} {
		if !strings.Contains(attach.errOut.String(), want) {
			t.Fatalf("attach showed no %q:\n%s", want, attach.errOut.String())
		}
	}
	if !strings.Contains(attach.out.String(), "finished") {
		t.Fatalf("attach showed no output:\n%s", attach.out.String())
	}
}

func TestKillCancelsARunAndForceEndsItAtOnce(t *testing.T) {
	state := t.TempDir()
	file := write(t, "idea.py", "import time, gongoozler as gz\nwith gz.stage('wait'):\n    time.sleep(60)\n")
	p := start(t, state, []string{pythonPath()}, "run", "--plain", "--launcher", "python3", file)
	p.waitFor(t, "▸ wait")
	id := runID.FindString(p.errOut.String())
	out, stderr, code := gz(t, state, nil, "kill", id)
	if code != 0 || !strings.Contains(out, id+": cancelled") {
		t.Fatalf("kill: %d %q %s", code, out, stderr)
	}
	p.wait(t, 10*time.Second)
	if out, _, _ := gz(t, state, nil, "kill", id); !strings.Contains(out, "already ended (cancelled)") {
		t.Fatalf("killing it again: %q", out)
	}
	// A run that ignores SIGTERM is cancelled with --force, at once.
	stubborn := write(t, "stubborn.py", "import signal, time, gongoozler as gz\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nwith gz.stage('wait'):\n    time.sleep(60)\n")
	q := start(t, state, []string{pythonPath()}, "run", "--plain", "--launcher", "python3", stubborn)
	q.waitFor(t, "▸ wait")
	id = runID.FindString(q.errOut.String())
	t0 := time.Now()
	out, stderr, code = gz(t, state, nil, "kill", "--force", id)
	if code != 0 || !strings.Contains(out, id+": cancelled") || time.Since(t0) > 5*time.Second {
		t.Fatalf("kill --force: %d %q %s (%v)", code, out, stderr, time.Since(t0))
	}
	q.wait(t, 10*time.Second)
}

// --------------------------------------------------------------------------------- results

const resultsFile = `import gongoozler as gz
from gongoozler import results
p = gz.params(x=1)
results.write_json("stats.json", {"score": p.x * p.x - 4 * p.x, "x": p.x})
results.write_jsonl("rows.jsonl", [{"i": i, "v": i * p.x} for i in range(5)])
`

func TestARerunReproducesItsResultsAndDiffShowsWhatChanged(t *testing.T) {
	state := t.TempDir()
	file := write(t, "idea.py", resultsFile)
	env := []string{pythonPath()}
	if _, stderr, code := gz(t, state, env, "run", "--plain", "--launcher", "python3", file); code != 0 {
		t.Fatal(stderr)
	}
	first := lsJSON(t, state)[0].ID
	out, stderr, code := gz(t, state, env, "rerun", "--plain", "--launcher", "python3", first)
	if code != 0 || !strings.Contains(out, "reproduced: 2 results byte for byte") {
		t.Fatalf("rerun: %d\n%s\n%s", code, out, stderr)
	}
	if _, stderr, code := gz(t, state, env, "run", "--plain", "--launcher", "python3", "--param", "x=5", file); code != 0 {
		t.Fatal(stderr)
	}
	all := lsJSON(t, state)
	third := all[len(all)-1].ID
	out, _, code = gz(t, state, nil, "diff", first, third)
	if code != 1 {
		t.Fatalf("diff of different runs: exit %d\n%s", code, out)
	}
	for _, want := range []string{"param x: (unset) → 5", "2 differ", "stats.json: line 1: score, x", "rows.jsonl: line 2: v"} {
		if !strings.Contains(out, want) {
			t.Fatalf("no %q in:\n%s", want, out)
		}
	}
	// A rerun of a file changed since is run from the copy kept with the run.
	if err := os.WriteFile(file, []byte(resultsFile+"\nprint('changed')\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	out, stderr, code = gz(t, state, env, "rerun", "--plain", "--launcher", "python3", first)
	if code != 0 || !strings.Contains(stderr, "has changed since run") {
		t.Fatalf("rerun of a changed file: %d\n%s\n%s", code, out, stderr)
	}
}

func TestASweepRunsEveryPointAndRanksThem(t *testing.T) {
	state := t.TempDir()
	file := write(t, "idea.py", resultsFile)
	env := []string{pythonPath()}
	out, stderr, code := gz(t, state, env, "sweep", "--plain", "--launcher", "python3", "--param", "x=0,1,2,3,4", "--rank", "-score", "--jobs", "3", file)
	if code != 0 {
		t.Fatalf("sweep: %d\n%s", code, stderr)
	}
	// score = x² - 4x: lowest at x=2, then 1 and 3, then 0 and 4.
	var order []string
	for _, line := range strings.Split(strings.TrimSpace(out), "\n")[1:] {
		order = append(order, strings.Fields(line)[len(strings.Fields(line))-1])
	}
	if strings.Join(order[:3], " ") != "x=2 x=1 x=3" || len(order) != 5 {
		t.Fatalf("ranked:\n%s", out)
	}
	if !strings.Contains(stderr, "[5/5] ok") || !strings.Contains(stderr, "5 ok, 0 not") {
		t.Fatalf("sweep progress:\n%s", stderr)
	}
	all := lsJSON(t, state)
	if len(all) != 5 || all[0].Sweep == "" || all[0].Sweep != all[4].Sweep {
		t.Fatalf("the runs do not name their sweep: %+v", all)
	}
	rec, err := os.ReadFile(filepath.Join(state, "sweeps", all[0].Sweep+".json"))
	if err != nil || !strings.Contains(string(rec), `"rank": "-score"`) {
		t.Fatalf("the sweep's record: %v %s", err, rec)
	}
	// As JSON: each run's end, then the sweep's, ranked highest first.
	out, _, code = gz(t, state, env, "sweep", "--json", "--launcher", "python3", "--param", "x=[0, 5]", "--rank", "score", file)
	evs := decodeLines(t, out)
	if code != 0 || len(evs) != 3 || evs[2]["kind"] != "sweep.end" {
		t.Fatalf("sweep --json: %d %v", code, evs)
	}
	ranked := evs[2]["runs"].([]any)
	if top := ranked[0].(map[string]any); top["value"].(float64) != 5 {
		t.Fatalf("ranked first: %v", top)
	}
}

func TestASweepWithAFailingPointSaysSo(t *testing.T) {
	file := write(t, "idea.py", "import gongoozler as gz\np = gz.params(x=1)\nassert p.x != 2, 'two is bad'\n")
	_, stderr, code := gz(t, t.TempDir(), []string{pythonPath()}, "sweep", "--plain", "--launcher", "python3", "--param", "x=1,2,3", file)
	if code != 1 || !strings.Contains(stderr, "2 ok, 1 not") {
		t.Fatalf("exit %d:\n%s", code, stderr)
	}
}

// --------------------------------------------------------------------------------- the server

// serve starts a server and a worker on it, both from the CLI, and returns the server's URL.
func serve(t *testing.T, workerArgs ...string) string {
	t.Helper()
	srv := start(t, t.TempDir(), []string{"GONGOOZLER_TOKEN=secret"}, "server", "--listen", "127.0.0.1:0", "--ttl", "2s")
	var url string
	for deadline := time.Now().Add(10 * time.Second); url == ""; time.Sleep(10 * time.Millisecond) {
		if m := regexp.MustCompile(`http://127\.0\.0\.1:\d+`).FindString(srv.out.String()); m != "" {
			url = m
		}
		if time.Now().After(deadline) {
			t.Fatalf("the server did not start:\n%s", srv.errOut.String())
		}
	}
	args := append([]string{"worker", "--server", url, "--launcher", "python3", "--cgroup", "off", "--heartbeat", "200ms"}, workerArgs...)
	start(t, t.TempDir(), []string{"GONGOOZLER_TOKEN=secret", pythonPath()}, args...)
	return url
}

func TestAFileRunOnTheServerKeepsTheSameResultsAsHere(t *testing.T) {
	url := serve(t)
	state := t.TempDir()
	file := write(t, "idea.py", resultsFile)
	env := []string{"GONGOOZLER_TOKEN=secret", pythonPath()}
	_, stderr, code := gz(t, state, env, "run", "--plain", "--remote", "--server", url, "--param", "x=2", file)
	if code != 0 {
		t.Fatalf("remote run: exit %d\n%s", code, stderr)
	}
	for _, want := range []string{"queued on the server", "running on worker", " ok in "} {
		if !strings.Contains(stderr, want) {
			t.Fatalf("no %q in:\n%s", want, stderr)
		}
	}
	if _, stderr, code := gz(t, state, env, "run", "--plain", "--launcher", "python3", "--param", "x=2", file); code != 0 {
		t.Fatal(stderr)
	}
	all := lsJSON(t, state)
	if len(all) != 2 || all[0].Placement != "remote" || all[0].Status != "ok" || all[1].Placement != "local" {
		t.Fatalf("listed %+v", all)
	}
	out, _, code := gz(t, state, nil, "diff", all[0].ID, all[1].ID)
	if code != 0 || !strings.Contains(out, "2 the same, 0 differ") {
		t.Fatalf("the box and here differ:\n%s", out)
	}
	// Its events are here too, as the server numbered them, and from the server.
	local, _, _ := gz(t, state, nil, "events", all[0].ID)
	evs := decodeLines(t, local)
	if len(evs) < 5 || evs[0]["kind"] != "job.queued" || evs[len(evs)-1]["kind"] != "run.end" {
		t.Fatalf("mirrored events: %v ... %v", evs[0], evs[len(evs)-1])
	}
	if out, _, code := gz(t, state, env, "pull", all[0].ID); code != 0 || !strings.Contains(out, " ok: ") {
		t.Fatalf("pull: %d %s", code, out)
	}
	if out, _, code := gz(t, state, env, "ls", "--remote", "--server", url, "--json"); code != 0 || !strings.Contains(out, all[0].ID) {
		t.Fatalf("ls --remote: %d %s", code, out)
	}
	// Without the token the server refuses.
	if _, stderr, code := gz(t, state, []string{"GONGOOZLER_TOKEN=wrong"}, "ls", "--remote", "--server", url); code == 0 || !strings.Contains(stderr, "bearer token") {
		t.Fatalf("a wrong token: %d %s", code, stderr)
	}
}

func TestARemoteRunIsCancelledWithKill(t *testing.T) {
	url := serve(t)
	state := t.TempDir()
	env := []string{"GONGOOZLER_TOKEN=secret", "GONGOOZLER_SERVER=" + url}
	file := write(t, "idea.py", "import time, gongoozler as gz\nwith gz.stage('wait'):\n    time.sleep(60)\n")
	out, stderr, code := gz(t, state, env, "run", "--remote", "--detach", file)
	if code != 0 {
		t.Fatalf("detach: %d %s", code, stderr)
	}
	id := strings.TrimSpace(out)
	a := start(t, state, env, "attach", "--plain", id)
	a.waitFor(t, "▸ wait")
	if out, stderr, code := gz(t, state, env, "kill", id); code != 0 || !strings.Contains(out, "cancelling on the server") {
		t.Fatalf("kill: %d %s %s", code, out, stderr)
	}
	a.wait(t, 30*time.Second)
	if !strings.Contains(a.errOut.String(), " cancelled in ") {
		t.Fatalf("attach saw:\n%s", a.errOut.String())
	}
	if all := lsJSON(t, state); all[0].Status != "cancelled" {
		t.Fatalf("listed as %s", all[0].Status)
	}
}

func TestAFileDeclaringMoreMemoryThanThisMachineHasGoesToTheServer(t *testing.T) {
	url := serve(t, "--memory", "1PiB")
	state := t.TempDir()
	file := write(t, "big.py", "# /// script\n# dependencies = []\n# [tool.gongoozler]\n# memory = \"512TiB\"\n# ///\nprint('big')\n")
	_, stderr, code := gz(t, state, []string{"GONGOOZLER_TOKEN=secret", "GONGOOZLER_SERVER=" + url}, "run", "--plain", "--no-lock", file)
	if code != 0 || !strings.Contains(stderr, "runs on the server: it declares 512.0 TiB of memory") {
		t.Fatalf("exit %d:\n%s", code, stderr)
	}
	if all := lsJSON(t, state); len(all) != 1 || all[0].Placement != "remote" {
		t.Fatalf("listed %+v", all)
	}
	// A file that says where it runs is run there.
	here := write(t, "here.py", "# /// script\n# dependencies = []\n# [tool.gongoozler]\n# memory = \"512TiB\"\n# placement = \"local\"\n# ///\nprint('here')\n")
	_, stderr, code = gz(t, state, []string{"GONGOOZLER_SERVER=" + url, "GONGOOZLER_TOKEN=secret"}, "run", "--plain", "--no-lock", "--launcher", "python3", "--cgroup", "off", here)
	if code != 0 || strings.Contains(stderr, "on the server") {
		t.Fatalf("placement local: exit %d:\n%s", code, stderr)
	}
}
