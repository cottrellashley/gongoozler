package supervise

import (
	"context"
	"encoding/json"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/procmem"
	"github.com/cottrellashley/gongoozler/internal/runs"
)

func pythonSrc() string {
	_, here, _, _ := runtime.Caller(0)
	return filepath.Join(filepath.Dir(here), "..", "..", "python", "src")
}

func script(t *testing.T, code string) string {
	p := filepath.Join(t.TempDir(), "run.py")
	if err := os.WriteFile(p, []byte(code), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

func run(t *testing.T, code string, change func(*Options)) (Result, []events.Event) {
	t.Helper()
	o := Options{File: script(t, code), State: t.TempDir(), Launcher: []string{"python3"}, Env: []string{"PYTHONPATH=" + pythonSrc()}}
	if change != nil {
		change(&o)
	}
	res, err := Run(context.Background(), o)
	if err != nil {
		t.Fatal(err)
	}
	return res, read(t, res.Dir)
}

func read(t *testing.T, dir string) []events.Event {
	f, err := os.Open(filepath.Join(dir, "events.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	var all []events.Event
	if err := events.Read(f, func(e events.Event) error { all = append(all, e); return nil }); err != nil {
		t.Fatal(err)
	}
	return all
}

func kinds(evs []events.Event) string {
	var out []string
	for _, e := range evs {
		switch e.Kind() {
		case "resource", "output":
		default:
			out = append(out, e.Kind())
		}
	}
	return strings.Join(out, " ")
}

func find(evs []events.Event, kind string) []events.Event {
	var out []events.Event
	for _, e := range evs {
		if e.Kind() == kind {
			out = append(out, e)
		}
	}
	return out
}

func end(t *testing.T, evs []events.Event) events.Event {
	t.Helper()
	e := find(evs, "run.end")
	if len(e) != 1 {
		t.Fatalf("%d run.end events", len(e))
	}
	return e[0]
}

func telOf(e events.Event) map[string]any { m, _ := e["telemetry"].(map[string]any); return m }

func TestARunsEventsAreStoredInProgramOrder(t *testing.T) {
	res, evs := run(t, `
import sys, time, gongoozler as gz
with gz.stage("pull"):
    gz.progress("rows", 5, 10)
    time.sleep(0.2)
    gz.progress("rows", 10, 10)
(gz.artifacts_dir() / "tearsheet.html").write_text("<html></html>")
gz.artifact(gz.artifacts_dir() / "tearsheet.html")
print("to stdout"); print("again", flush=True)
print("to stderr", file=sys.stderr)
`, func(o *Options) { o.Env = append(o.Env, "PWD="+t.TempDir()) })
	if res.Status != "ok" || res.ExitCode != 0 {
		t.Fatalf("status %s, exit %d", res.Status, res.ExitCode)
	}
	got := kinds(evs)
	if !strings.HasPrefix(got, "run.start hello stage progress") || !strings.Contains(got, "progress stage artifact.declare artifact") || !strings.HasSuffix(got, "run.end") {
		t.Fatalf("kinds: %s", got)
	}
	for i, e := range evs {
		if n, _ := e.Int("n"); n != int64(i+1) {
			t.Fatalf("event %d numbered %d", i, n)
		}
	}
	// Progress values before the stage ended all arrive before its end, and the last is 10.
	var last float64
	for _, e := range evs {
		if e.Kind() == "progress" {
			last, _ = e.Num("done")
		}
		if e.Kind() == "stage" && e.Str("phase") == "end" && last != 10 {
			t.Fatalf("the stage ended before its last progress (%v)", last)
		}
	}
	var stdout []string
	for _, e := range find(evs, "output") {
		if e.Str("stream") == "stdout" {
			stdout = append(stdout, e.Str("line"))
		}
	}
	if strings.Join(stdout, "|") != "to stdout|again" {
		t.Fatalf("stdout lines, in their order: %v", stdout)
	}
	art := find(evs, "artifact")[0]
	if art.Str("path") != filepath.Join(res.Dir, "artifacts", "tearsheet.html") || art.Str("sha256") == "" {
		t.Fatalf("artifact %v", art)
	}
	if !telOf(end(t, evs))["complete"].(bool) {
		t.Fatalf("telemetry %v", telOf(end(t, evs)))
	}
	m, err := runs.ReadMeta(filepath.Dir(filepath.Dir(res.Dir)), res.ID)
	if err != nil || m.Status != "ok" || m.Events != int64(len(evs)) {
		t.Fatalf("meta %+v %v", m, err)
	}
	if _, err := os.Stat(filepath.Join(res.Dir, "source", "run.py")); err != nil {
		t.Fatal("the file as run is not kept with the run")
	}
	for _, p := range []string{res.Dir, filepath.Join(res.Dir, "events.jsonl"), filepath.Join(res.Dir, "meta.json")} {
		if st, _ := os.Stat(p); st.Mode().Perm()&0o077 != 0 {
			t.Fatalf("%s is %v: readable by others", p, st.Mode().Perm())
		}
	}
}

func TestMetaIsWrittenWhenTheRunStarts(t *testing.T) {
	state := t.TempDir()
	done := make(chan Result)
	go func() {
		res, _ := Run(context.Background(), Options{File: script(t, "import time\ntime.sleep(1.5)\n"), State: state, Launcher: []string{"python3"}})
		done <- res
	}()
	time.Sleep(700 * time.Millisecond)
	all, err := runs.List(state)
	if err != nil || len(all) != 1 || all[0].Status != "running" || all[0].SupervisorPID != os.Getpid() {
		t.Fatalf("while running: %+v %v", all, err)
	}
	<-done
}

// C1: one non-finite value, or anything JSON cannot encode, must never cost the run its record.
func TestNoValueARunPassesCanBreakItsRecord(t *testing.T) {
	res, evs := run(t, `
import gongoozler as gz
gz.log("before")
gz.emit("custom", value=float("nan"), keys={(1, 2): float("inf")})
gz.params(threshold=float("nan"))
gz.log("after")
`, nil)
	if res.Status != "ok" {
		t.Fatalf("status %s", res.Status)
	}
	var msgs []string
	for _, e := range find(evs, "log") {
		msgs = append(msgs, e.Str("msg"))
	}
	if strings.Join(msgs, ",") != "before,after" || len(find(evs, "custom")) != 1 || len(find(evs, "hello")) != 1 {
		t.Fatalf("logs %v, kinds %s", msgs, kinds(evs))
	}
	if c := find(evs, "custom")[0]; c["value"] != nil {
		t.Fatalf("NaN stored as %v", c["value"])
	}
}

// C2: an event too large is cut to fit, counted, and the stream goes on.
func TestAHugeEventIsCutToFitAndTheStreamGoesOn(t *testing.T) {
	_, evs := run(t, `
import gongoozler as gz
gz.log("x" * 5_000_000)
with gz.stage("later"):
    gz.log("after")
`, nil)
	logs := find(evs, "log")
	if len(logs) != 2 || logs[1].Str("msg") != "after" || len(logs[0].Str("msg")) > 70_000 {
		t.Fatalf("logs: %d", len(logs))
	}
	tel := telOf(end(t, evs))
	if tel["complete"].(bool) || tel["truncated"].(json.Number).String() != "1" {
		t.Fatalf("telemetry %v", tel)
	}
	if !strings.Contains(end(t, evs).Str("note"), "1 events cut to fit") {
		t.Fatalf("note %q", end(t, evs).Str("note"))
	}
}

func TestAFailingRunReportsItsErrorTheStageItWasInAndItsExitCode(t *testing.T) {
	res, evs := run(t, `
import gongoozler as gz
with gz.stage("simulate"), gz.stage("window 3"):
    raise RuntimeError("margin call unmet")
`, nil)
	if res.Status != "failed" || res.ExitCode != 1 {
		t.Fatalf("status %s, exit %d", res.Status, res.ExitCode)
	}
	if e := find(evs, "error"); len(e) != 1 || e[0].Str("msg") != "margin call unmet" {
		t.Fatalf("error events: %v", e)
	}
	// The stages ended (with the error) on the way out; a crash leaves them open instead.
	if e := end(t, evs); e["open_stages"] != nil {
		t.Fatalf("open stages after an exception that unwound them: %v", e["open_stages"])
	}
}

func TestAKilledRunReportsTheStagesItWasIn(t *testing.T) {
	res, evs := run(t, `
import os, signal, time, gongoozler as gz
with gz.stage("simulate"), gz.stage("window 3"):
    gz.progress("sim", 1, 2)
    time.sleep(0.2)
    os.kill(os.getpid(), signal.SIGKILL)
`, nil)
	if res.Status != "killed" || res.Signal != "SIGKILL" || res.ExitCode != 137 {
		t.Fatalf("status %s, signal %s, exit %d", res.Status, res.Signal, res.ExitCode)
	}
	e := end(t, evs)
	if !strings.Contains(e.Str("note"), "it was in simulate › window 3") {
		t.Fatalf("note %q", e.Str("note"))
	}
	if procmem.ScopesAvailable() && !strings.Contains(e.Str("note"), "not for memory") {
		t.Fatalf("with a cgroup, a SIGKILL that was not for memory says so: %q", e.Str("note"))
	}
	if tel := telOf(e); tel["unclean_exits"].(json.Number).String() != "1" {
		t.Fatalf("a killed process did not close its ring: %v", tel)
	}
}

// An out-of-memory kill is exact with a cgroup: the run is limited, and the kernel's count says so.
func TestAnOutOfMemoryKillIsReportedExactly(t *testing.T) {
	if !procmem.ScopesAvailable() {
		t.Skip("no systemd user scopes here")
	}
	res, evs := run(t, `
import gongoozler as gz
with gz.stage("load everything"):
    hog = [bytearray(10_000_000) for _ in range(40)]
`, func(o *Options) { o.Memory = 100 << 20 })
	e := end(t, evs)
	if res.Status != "oom" || e.Str("memory_source") != "cgroup" {
		t.Fatalf("status %s, source %s, note %q", res.Status, e.Str("memory_source"), e.Str("note"))
	}
	if n, _ := e.Int("oom_kills"); n < 1 {
		t.Fatalf("oom_kills %d", n)
	}
	if !strings.Contains(e.Str("note"), "it was in load everything") {
		t.Fatalf("note %q", e.Str("note"))
	}
}

// H3: memory shared copy-on-write by forked workers is counted once, not once per worker.
func TestMemorySharedByForkedWorkersIsCountedOnce(t *testing.T) {
	code := `
import os, time
data = bytearray(150_000_000)
for i in range(0, len(data), 4096):
    data[i] = 1
kids = []
for _ in range(3):
    pid = os.fork()
    if pid == 0:
        time.sleep(2.5)
        os._exit(0)
    kids.append(pid)
time.sleep(1.5)
pss = 0
for p in [os.getpid(), *kids]:
    for line in open(f"/proc/{p}/smaps_rollup"):
        if line.startswith("Pss:"):
            pss += int(line.split()[1]) * 1024
print("pss", pss)
for p in kids:
    os.waitpid(p, 0)
`
	for _, mode := range []string{"auto", "off"} {
		t.Run(mode, func(t *testing.T) {
			res, evs := run(t, code, func(o *Options) { o.Cgroup = mode })
			var pss float64
			for _, e := range find(evs, "output") {
				if v, ok := strings.CutPrefix(e.Str("line"), "pss "); ok {
					pss, _ = strconv.ParseFloat(v, 64)
				}
			}
			ratio := float64(res.Peak) / pss
			if pss == 0 || ratio < 0.9 || ratio > 1.25 {
				t.Fatalf("%s: peak %d against %d actual (%.2fx); a sum of RSS gives 4x", end(t, evs).Str("memory_source"), res.Peak, int64(pss), ratio)
			}
		})
	}
}

// H1: whatever the run started is stopped when it ends, and named.
func TestProcessesTheRunStartedDoNotOutliveIt(t *testing.T) {
	pidFile := filepath.Join(t.TempDir(), "child.pid")
	_, evs := run(t, `
import subprocess
p = subprocess.Popen(["sleep", "41"])
open("`+pidFile+`", "w").write(str(p.pid))
`, func(o *Options) { o.Grace = 500 * time.Millisecond })
	b, err := os.ReadFile(pidFile)
	if err != nil {
		t.Fatal(err)
	}
	pid, _ := strconv.Atoi(string(b))
	if syscall.Kill(pid, 0) == nil {
		if st, err := procmem.Stat("/proc", pid); err == nil && st.State != 'Z' {
			syscall.Kill(pid, syscall.SIGKILL)
			t.Fatalf("sleep %d outlived the run", pid)
		}
	}
	left, _ := end(t, evs)["leftovers"].([]any)
	if len(left) != 1 || !strings.HasPrefix(left[0].(string), "sleep (") {
		t.Fatalf("leftovers %v", end(t, evs)["leftovers"])
	}
}

func TestATimeLimitStopsTheRun(t *testing.T) {
	res, _ := run(t, "import time\ntime.sleep(30)\n", func(o *Options) { o.Timeout = 300 * time.Millisecond })
	if res.Status != "timeout" || res.Wall > 10 {
		t.Fatalf("status %s after %.1fs", res.Status, res.Wall)
	}
}

func TestCancellingTheContextStopsTheRun(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()
	res, err := Run(ctx, Options{File: script(t, "import time\ntime.sleep(30)\n"), State: t.TempDir(), Launcher: []string{"python3"}})
	if err != nil || res.Status != "cancelled" {
		t.Fatalf("status %s, %v", res.Status, err)
	}
}

func TestEventsReachTheLogWithinAQuarterSecond(t *testing.T) {
	_, evs := run(t, `
import time, gongoozler as gz
for i in range(200):
    gz.emit("tick", i=i)
    time.sleep(0.005)
`, nil)
	var lat []float64
	for _, e := range find(evs, "tick") {
		t0, _ := e.Num("t")
		rt, _ := e.Num("rt")
		lat = append(lat, rt-t0)
	}
	sort.Float64s(lat)
	p99 := lat[int(math.Ceil(0.99*float64(len(lat))))-1]
	if len(lat) != 200 || p99 > 0.25 {
		t.Fatalf("%d events, p99 latency %.3fs", len(lat), p99)
	}
}

func TestALibraryOfAnotherVersionIsFlagged(t *testing.T) {
	_, evs := run(t, "import gongoozler as gz\ngz.log('x')\n", func(o *Options) { o.Version = "9.9.9" })
	if v := find(evs, "cli.version"); len(v) != 1 || !strings.Contains(v[0].Str("error"), "this supervisor is 9.9.9") {
		t.Fatalf("version events %v", v)
	}
}

func TestARecordThatIsNotAnEventIsCountedAndTheRunGoesOn(t *testing.T) {
	res, evs := run(t, `
import gongoozler as gz
from gongoozler import _events
gz.log("first")
em = _events._get()
with em._lock:
    em.transport.ring.write(b"not json")
    em.transport.ring.write(b'{"v":2,"kind":"x","t":1,"seq":99}')
gz.log("after")
`, nil)
	if res.Status != "ok" {
		t.Fatalf("status %s", res.Status)
	}
	if p := find(evs, "cli.protocol"); len(p) != 2 || !strings.Contains(p[1].Str("error"), "protocol 2") {
		t.Fatalf("protocol events: %v", p)
	}
	if len(find(evs, "log")) != 2 {
		t.Fatal("events around the bad records were lost")
	}
	if n, _ := end(t, evs).Int("protocol_errors"); n != 2 {
		t.Fatalf("protocol_errors %d", n)
	}
}

func TestOutputBeyondTheCapGoesOnlyToTheLogFile(t *testing.T) {
	res, evs := run(t, "for i in range(20): print(i)\n", func(o *Options) { o.OutputCap = 5 })
	if n := len(find(evs, "output")); n != 5 {
		t.Fatalf("%d output events", n)
	}
	b, err := os.ReadFile(filepath.Join(res.Dir, "output.log"))
	if err != nil || strings.Count(string(b), "\n") != 20 {
		t.Fatalf("output.log: %q %v", b, err)
	}
	if note := end(t, evs).Str("note"); !strings.Contains(note, "15 output lines are only in output.log") {
		t.Fatalf("note: %q", note)
	}
}

func TestALauncherThatCannotStartFailsCleanly(t *testing.T) {
	res, evs := run(t, "print('never')\n", func(o *Options) { o.Launcher = []string{"/nonexistent/launcher"} })
	if res.Status != "failed" || res.ExitCode != 127 {
		t.Fatalf("status %s exit %d", res.Status, res.ExitCode)
	}
	if note := end(t, evs).Str("note"); !strings.Contains(note, "cannot start") {
		t.Fatalf("note %q", note)
	}
}

func TestStartupIsMeasured(t *testing.T) {
	_, evs := run(t, "import gongoozler as gz\ngz.log('x')\n", nil)
	if s, ok := end(t, evs).Num("startup"); !ok || s <= 0 || s > 10 {
		t.Fatalf("startup %v", end(t, evs)["startup"])
	}
}

// helperOptions is what a test hands the test binary, acting as a supervisor of its own.
type helperOptions struct {
	File, State, Cgroup string
	Launcher, Env       []string
}

// TestMain lets the test binary act as a supervisor of its own, for the tests that kill one.
func TestMain(m *testing.M) {
	if spec := os.Getenv("GZ_TEST_SUPERVISOR"); spec != "" {
		var h helperOptions
		if err := json.Unmarshal([]byte(spec), &h); err != nil {
			panic(err)
		}
		// The test kills this process mid-run: what Run returns is never seen.
		_, _ = Run(context.Background(), Options{File: h.File, State: h.State, Cgroup: h.Cgroup, Launcher: h.Launcher, Env: h.Env})
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func gone(pid int) bool {
	st, err := procmem.Stat("/proc", pid)
	return err != nil || st.State == 'Z'
}

func TestARunDiesWithinASecondOfItsSupervisor(t *testing.T) {
	cases := map[string]string{
		// In a cgroup of its own, the scope's anchor kills the run whatever it ignores.
		"cgroup": "signal.signal(signal.SIGTERM, signal.SIG_IGN)",
		// With none, the parent-death signal reaches the leader, which must not ignore it.
		"no cgroup": "",
	}
	for name, ignore := range cases {
		t.Run(name, func(t *testing.T) {
			cgroup := "off"
			if name == "cgroup" {
				if !procmem.ScopesAvailable() {
					t.Skip("systemd user scopes are not available here")
				}
				cgroup = "require"
			}
			dir, state := t.TempDir(), t.TempDir()
			pidfile := filepath.Join(dir, "pid")
			file := script(t, "import os, signal, time\n"+ignore+"\nopen("+strconv.Quote(pidfile)+", 'w').write(str(os.getpid()))\ntime.sleep(60)\n")
			spec, _ := json.Marshal(helperOptions{File: file, State: state, Cgroup: cgroup, Launcher: []string{"python3"}, Env: []string{"PYTHONPATH=" + pythonSrc()}})
			helper := exec.Command(os.Args[0], "-test.run=^$")
			helper.Env = append(os.Environ(), "GZ_TEST_SUPERVISOR="+string(spec))
			if err := helper.Start(); err != nil {
				t.Fatal(err)
			}
			var pid int
			for deadline := time.Now().Add(10 * time.Second); pid == 0 && time.Now().Before(deadline); time.Sleep(10 * time.Millisecond) {
				b, _ := os.ReadFile(pidfile)
				pid, _ = strconv.Atoi(string(b))
			}
			if pid == 0 {
				helper.Process.Kill()
				t.Fatal("the run never started")
			}
			helper.Process.Kill()
			helper.Wait()
			killed := time.Now()
			for !gone(pid) && time.Since(killed) < 2*time.Second {
				time.Sleep(5 * time.Millisecond)
			}
			if !gone(pid) {
				syscall.Kill(pid, syscall.SIGKILL)
				t.Fatal("the run outlived its supervisor")
			}
			if took := time.Since(killed); took > time.Second {
				t.Fatalf("the run took %v to die", took)
			}
			all, _ := runs.List(state)
			if len(all) != 1 || all[0].Status != "lost" {
				t.Fatalf("listed as %+v, want lost", all)
			}
			settled := Settle(state, "/proc")
			if len(settled) != 1 || settled[0].Status != "lost" || settled[0].Ended.IsZero() {
				t.Fatalf("settled %+v", settled)
			}
			if m, _ := runs.ReadMeta(state, all[0].ID); m.Status != "lost" {
				t.Fatalf("meta says %s after settling", m.Status)
			}
			if _, err := os.Stat(filepath.Join("/dev/shm", "gongoozler-"+strconv.Itoa(os.Getuid()), all[0].ID)); err == nil {
				t.Fatal("the lost run's rings are still there")
			}
		})
	}
}
