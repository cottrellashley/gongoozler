// Package supervise runs a file and owns everything about the run (spec/supervisor.md):
//   - its process tree: the run starts in a process group of its own, dies with the supervisor
//     (the kernel's parent-death signal), and leaves nothing behind (leftovers are stopped, and
//     orphans come back to the supervisor, a subreaper, to be reaped);
//   - its accounting: a cgroup of its own where systemd can make one (exact memory, an exact
//     out-of-memory count, limits that hold), and summed PSS where it cannot;
//   - its record: the run's shared-memory rings, read from outside it, and an event log flushed on a
//     ticker and at once after what matters, with meta.json written when the run starts;
//   - its artifacts: copied and hashed outside the run.
//
// The CLI runs files with it locally, sweeps run many with it, and a box's worker runs it too.
package supervise

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/procmem"
	"github.com/cottrellashley/gongoozler/internal/runs"
	"github.com/cottrellashley/gongoozler/internal/term"
)

// Options describe one run.
type Options struct {
	File   string
	Args   []string
	Params map[string]any
	// State holds the runs: STATE/runs/ID.
	State string
	// ID is the run's ID; a worker uses the server's. Default: a new one.
	ID string
	// Launcher is the command the file is given to. Default: uv run --quiet --script.
	Launcher []string
	// Uv says the launcher is uv, which forwards signals to its child and reports a child killed
	// by a signal as 128 + the signal. Default: true when the launcher's program is named uv.
	Uv  *bool
	Env []string
	// Memory and CPUs limit the run when it has a cgroup of its own; 0 means no limit.
	Memory  int64
	CPUs    float64
	Timeout time.Duration
	// Cgroup is "auto" (a systemd scope where one can be made), "off", or "require".
	Cgroup string
	// RingBase holds the run's rings: /dev/shm when it is writable, else the run directory.
	RingBase string
	// Signals forwards SIGINT, SIGTERM and SIGHUP to the run.
	Signals bool
	Render  *term.Renderer
	// OnEvent sees every stored event, in order. It must not block.
	OnEvent func(events.Event)
	// OutputCap is how many output lines are stored as events (all go to output.log).
	OutputCap int
	// Grace is how long leftovers get between SIGTERM and SIGKILL.
	Grace time.Duration
	// Poll is how often the rings are read.
	Poll    time.Duration
	Proc    string
	Version string
	// Placement and Sweep are recorded in the run's meta: where it was placed, and the sweep it
	// belongs to.
	Placement string
	Sweep     string
}

// Result is how a run ended.
type Result struct {
	runs.Meta
	Dir string
}

// Code is the exit code the CLI should end with: the run's own, 128 + the signal for a kill.
func (r Result) Code() int { return r.ExitCode }

func (o *Options) defaults() {
	if len(o.Launcher) == 0 {
		o.Launcher = []string{uvBinary(), "run", "--quiet", "--script"}
	}
	if o.Uv == nil {
		uv := filepath.Base(o.Launcher[0]) == "uv"
		o.Uv = &uv
	}
	if o.Proc == "" {
		o.Proc = "/proc"
	}
	if o.OutputCap == 0 {
		o.OutputCap = 10_000
	}
	if o.Grace == 0 {
		o.Grace = 2 * time.Second
	}
	if o.Poll == 0 {
		o.Poll = 25 * time.Millisecond
	}
	if o.Cgroup == "" {
		o.Cgroup = "auto"
	}
	if o.Version == "" {
		o.Version = "dev"
	}
}

func uvBinary() string {
	if p := os.Getenv("GONGOOZLER_UV"); p != "" {
		return p
	}
	return "uv"
}

// Run runs the file and blocks until it has ended, everything it started is gone, and every event
// is stored.
func Run(ctx context.Context, o Options) (Result, error) {
	o.defaults()
	becomeSubreaper()
	file, err := filepath.Abs(o.File)
	if err != nil {
		return Result{}, err
	}
	sum, err := fileSHA256(file)
	if err != nil {
		return Result{}, fmt.Errorf("cannot read %s: %w", o.File, err)
	}
	lock, _ := fileSHA256(file + ".lock")
	started := time.Now()
	id := o.ID
	if id == "" {
		id = runs.NewID(started)
	}
	dir := runs.Dir(o.State, id)
	for _, d := range []string{dir, filepath.Join(dir, "artifacts"), filepath.Join(dir, "source")} {
		if err := os.MkdirAll(d, 0o700); err != nil {
			return Result{}, err
		}
	}
	// The copies let `gongoozler rerun` run the file as it was, once it has changed; a run goes on
	// without them.
	_ = copyFile(file, filepath.Join(dir, "source", filepath.Base(file)))
	if lock != "" {
		_ = copyFile(file+".lock", filepath.Join(dir, "source", filepath.Base(file)+".lock"))
	}
	host, _ := os.Hostname()
	self, _ := procmem.Stat(o.Proc, os.Getpid())
	cwd, _ := os.Getwd()
	meta := runs.Meta{
		ID: id, File: file, SHA256: sum, Lock: lock, Args: nonNilS(o.Args), Params: o.Params, Started: started, Status: "running",
		Host: host, Cwd: cwd, SupervisorPID: os.Getpid(), SupervisorStart: self.StartTime, Version: o.Version, Placement: o.Placement, Sweep: o.Sweep,
	}
	if err := runs.WriteMeta(o.State, meta); err != nil {
		return Result{}, err
	}
	active.Store(id, true)
	defer active.Delete(id)
	log, err := events.Create(filepath.Join(dir, "events.jsonl"))
	if err != nil {
		return Result{}, err
	}
	output, err := os.OpenFile(filepath.Join(dir, "output.log"), os.O_CREATE|os.O_WRONLY|os.O_EXCL, 0o600)
	if err != nil {
		return Result{}, err
	}
	defer output.Close()
	ringDir, err := makeRingDir(o, id, dir)
	if err != nil {
		return Result{}, err
	}
	defer os.RemoveAll(ringDir)

	pl := newPipeline(log, o, filepath.Join(dir, "artifacts"))
	pl.cli("run.start", map[string]any{
		"id": id, "file": file, "sha256": sum, "lock_sha256": lock, "args": nonNilS(o.Args), "params": nonNil(o.Params),
		"launcher": o.Launcher, "host": host, "cwd": cwd, "pid": os.Getpid(), "version": o.Version,
	})

	paramsJSON, err := json.Marshal(nonNil(o.Params))
	if err != nil {
		return Result{}, err
	}
	env := append(os.Environ(),
		"GONGOOZLER_RING="+ringDir,
		"GONGOOZLER_RUN_ID="+id,
		"GONGOOZLER_ARTIFACTS="+filepath.Join(dir, "artifacts"),
		"GONGOOZLER_PARAMS="+string(paramsJSON),
	)
	if os.Getenv("PYTHONUNBUFFERED") == "" {
		env = append(env, "PYTHONUNBUFFERED=1") // a live view needs output as it is printed
	}
	env = append(env, o.Env...)
	out := newOutput(output, pl, o)

	tel := newTelemetry(ringDir, o, pl, id)
	go tel.loop()

	p, startErr := launch(o, id, append(append([]string{}, o.Launcher[1:]...), append([]string{file}, o.Args...)...), env, out.writer("stdout"), out.writer("stderr"))
	if startErr != nil {
		tel.final()
		pl.cli("run.end", map[string]any{"id": id, "status": "failed", "exit_code": 127, "wall": 0.0, "peak": 0, "note": "cannot start: " + startErr.Error()})
		meta.Status, meta.ExitCode = "failed", 127
		return finish(o, pl, meta, dir)
	}
	pl.process(p.pid)
	if st, err := procmem.Stat(o.Proc, p.pid); err == nil {
		meta.RunPID, meta.RunStart = p.pid, st.StartTime
		// So `gongoozler kill --force` can find the run without its supervisor; the run goes on without it.
		_ = runs.WriteMeta(o.State, meta)
	}
	mem := watchMemory(ctx, o, p, pl)
	stopReaper := reapOrphans(o.Proc, p)
	intr := handleSignals(ctx, o, p)

	<-p.waitDone
	intr.stop()
	stopReaper()
	leftovers := p.cleanup(o.Grace)
	sample, cgPeak, oomKills := mem.stop()
	sample.Peak = max(sample.Peak, p.maxRSS())
	if p.cgroup != nil {
		// The cgroup's own numbers are exact. The anchor keeps it until release; what was last read
		// stands if it went anyway.
		sample.Peak = max(sample.Peak, cgPeak)
		if pk, err := p.cgroup.Peak(); err == nil {
			sample.Peak = pk
		}
		if n, err := p.cgroup.OOMKills(); err == nil {
			oomKills = max(oomKills, n)
		}
	}
	st := p.status(*o.Uv, intr.reason(), oomKills)
	p.release()
	summary := tel.final()
	pl.waitArtifacts()

	wall := time.Since(started).Seconds()
	end := map[string]any{
		"id": id, "status": st.status, "exit_code": st.code, "wall": wall, "peak": sample.Peak, "memory_source": mem.source,
		"oom_kills": oomKills, "leftovers": leftovers, "telemetry": summary.fields(), "protocol_errors": summary.ProtocolErrors,
	}
	if st.signal != "" {
		end["signal"] = st.signal
	}
	if h := pl.firstHello(); !h.IsZero() {
		end["startup"] = h.Sub(started).Seconds()
	}
	var notes []string
	if st.note != "" {
		notes = append(notes, st.note)
	}
	if st.status != "ok" && len(summary.OpenStages) > 0 {
		end["open_stages"] = summary.OpenStages
		notes = append(notes, "it was in "+strings.Join(summary.OpenStages, " › "))
	}
	if !summary.Complete() {
		notes = append(notes, "telemetry incomplete: "+summary.describe())
	}
	if n := out.stored() - int64(o.OutputCap); n > 0 {
		notes = append(notes, fmt.Sprintf("%d output lines are only in output.log", n))
	}
	if err := out.failed(); err != nil {
		notes = append(notes, "output.log could not be written: "+err.Error())
	}
	if len(leftovers) > 0 {
		notes = append(notes, fmt.Sprintf("stopped %d leftover processes: %s", len(leftovers), strings.Join(leftovers, ", ")))
	}
	if len(notes) > 0 {
		meta.Note = strings.Join(notes, "; ")
		end["note"] = meta.Note
	}
	pl.cli("run.end", end)
	meta.Status, meta.ExitCode, meta.Signal, meta.Wall, meta.Peak = st.status, st.code, st.signal, wall, sample.Peak
	meta.MemorySource, meta.OOMKills, meta.Leftovers, meta.Telemetry = mem.source, oomKills, len(leftovers), summary.fields()
	return finish(o, pl, meta, dir)
}

func finish(o Options, pl *pipeline, meta runs.Meta, dir string) (Result, error) {
	err := pl.close()
	meta.Events = pl.count()
	meta.Ended = time.Now()
	if o.Render != nil {
		o.Render.Close()
	}
	if werr := runs.WriteMeta(o.State, meta); err == nil {
		err = werr
	}
	return Result{meta, dir}, err
}

// makeRingDir is where the run's processes put their rings: memory (/dev/shm, private to this
// user) when it is writable, otherwise the run directory.
func makeRingDir(o Options, id, dir string) (string, error) {
	d := filepath.Join(dir, "ring")
	base := o.RingBase
	if base == "" && writable("/dev/shm") {
		base = filepath.Join("/dev/shm", fmt.Sprintf("gongoozler-%d", os.Getuid()))
	}
	if base != "" {
		d = filepath.Join(base, id)
	}
	return d, os.MkdirAll(d, 0o700)
}

func writable(dir string) bool {
	f, err := os.CreateTemp(dir, ".gongoozler-probe-")
	if err != nil {
		return false
	}
	f.Close()
	os.Remove(f.Name())
	return true
}

func fileSHA256(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o600)
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

func nonNil(m map[string]any) map[string]any {
	if m == nil {
		return map[string]any{}
	}
	return m
}

func nonNilS(s []string) []string {
	if s == nil {
		return []string{}
	}
	return s
}
