package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/cottrellashley/gongoozler/internal/client"
	"github.com/cottrellashley/gongoozler/internal/header"
	"github.com/cottrellashley/gongoozler/internal/procmem"
	"github.com/cottrellashley/gongoozler/internal/runs"
	"github.com/cottrellashley/gongoozler/internal/supervise"
	"github.com/cottrellashley/gongoozler/internal/term"
	"github.com/cottrellashley/gongoozler/internal/version"
)

// runOptions are the flags `run` and `sweep` share: how a file is sized, placed and run.
type runOptions struct {
	memory, cpus, timeout string
	cgroup                string
	noLock                bool
	local, remote         bool
	server                string
	launcher              string
}

func (ro *runOptions) register(fs interface {
	StringVar(*string, string, string, string)
	BoolVar(*bool, string, bool, string)
}) {
	fs.StringVar(&ro.memory, "memory", "", "the run's memory limit, such as 48GiB (default: the file's [tool.gongoozler] memory)")
	fs.StringVar(&ro.cpus, "cpus", "", "the run's CPU limit (default: the file's cpus)")
	fs.StringVar(&ro.timeout, "timeout", "", "the run's time limit, such as 3h (default: the file's timeout)")
	fs.StringVar(&ro.cgroup, "cgroup", "auto", "auto: a cgroup of its own where systemd can make one; off; or require")
	fs.BoolVar(&ro.noLock, "no-lock", false, "do not lock the file's dependencies on its first run")
	fs.BoolVar(&ro.local, "local", false, "run here, whatever the file or $GONGOOZLER_PLACEMENT say")
	fs.BoolVar(&ro.remote, "remote", false, "run on the server")
	fs.StringVar(&ro.server, "server", "", "the server's URL (default $GONGOOZLER_SERVER)")
	fs.StringVar(&ro.launcher, "launcher", "", "the command to give the file to, instead of `uv run --script` (for tests)")
}

// plan is a file, read and sized, with where it will run.
type plan struct {
	file      string
	header    header.Header
	settings  header.Settings
	placement string // local or remote
	why       string
	client    *client.Client
	launcher  []string
}

// prepare reads the file's header, applies the flags, places the run and, on its first run, locks
// the file's dependencies.
func (ro *runOptions) prepare(file string, stderr io.Writer) (*plan, error) {
	abs, err := filepath.Abs(file)
	if err != nil {
		return nil, err
	}
	if _, err := os.Stat(abs); err != nil {
		return nil, fmt.Errorf("no file %s", file)
	}
	h, err := header.Read(abs)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", file, err)
	}
	p := &plan{file: abs, header: h, settings: h.Settings}
	s := &p.settings
	if ro.memory != "" {
		if s.Memory, err = header.Bytes(ro.memory); err != nil {
			return nil, fmt.Errorf("--memory: %w", err)
		}
	}
	if ro.cpus != "" {
		if s.CPUs, err = strconv.ParseFloat(ro.cpus, 64); err != nil || s.CPUs <= 0 {
			return nil, fmt.Errorf("--cpus: %q is not a positive number", ro.cpus)
		}
	}
	if ro.timeout != "" {
		if s.Timeout, err = header.Duration(ro.timeout); err != nil {
			return nil, fmt.Errorf("--timeout: %w", err)
		}
	}
	switch ro.cgroup {
	case "auto", "off", "require":
	default:
		return nil, fmt.Errorf("--cgroup is auto, off or require, not %q", ro.cgroup)
	}
	if ro.local && ro.remote {
		return nil, errors.New("--local or --remote, not both")
	}
	if ro.server != "" {
		p.client = client.New(ro.server, os.Getenv("GONGOOZLER_TOKEN"))
	} else {
		p.client = client.FromEnv()
	}
	if ro.launcher != "" {
		p.launcher = strings.Fields(ro.launcher)
	}
	avail, _ := procmem.Available("/proc")
	p.placement, p.why, err = place(ro, s, os.Getenv("GONGOOZLER_PLACEMENT"), p.client != nil, avail)
	if err != nil {
		return nil, err
	}
	if !ro.noLock && p.launcher == nil && h.Found {
		ensureLock(abs, stderr)
	}
	return p, nil
}

// place decides where a run goes: the flags first, then the file's placement, then
// $GONGOOZLER_PLACEMENT; "auto" goes remote only when the file declares more memory than this
// machine has available and a server is known.
func place(ro *runOptions, s *header.Settings, env string, haveServer bool, avail int64) (string, string, error) {
	choice, from := "auto", ""
	switch {
	case ro.local:
		choice, from = "local", "--local"
	case ro.remote:
		choice, from = "remote", "--remote"
	case s.Placement == "local" || s.Placement == "remote":
		choice, from = s.Placement, "the file's placement"
	case env == "local" || env == "remote":
		choice, from = env, "$GONGOOZLER_PLACEMENT"
	case env != "" && env != "auto":
		return "", "", fmt.Errorf("$GONGOOZLER_PLACEMENT is auto, local or remote, not %q", env)
	}
	if choice == "remote" && !haveServer {
		return "", "", fmt.Errorf("%s says remote, and no server is set: --server URL or $GONGOOZLER_SERVER", from)
	}
	if choice != "auto" {
		return choice, from, nil
	}
	if s.Memory > 0 && avail > 0 && s.Memory > avail {
		need := fmt.Sprintf("it declares %s of memory, and this machine has %s available", term.Bytes(float64(s.Memory)), term.Bytes(float64(avail)))
		if haveServer {
			return "remote", need, nil
		}
		return "local", need + "; no server is set, so it runs here", nil
	}
	return "local", "", nil
}

// ensureLock locks a file's dependencies on its first run (`uv lock --script`), so every later run
// of it, here or in a box, gets the same packages. A file with no script block has nothing to lock.
func ensureLock(file string, stderr io.Writer) {
	lock := file + ".lock"
	if _, err := os.Stat(lock); err == nil {
		return
	}
	uv := os.Getenv("GONGOOZLER_UV")
	if uv == "" {
		uv = "uv"
	}
	out, err := exec.Command(uv, "lock", "--script", file, "--quiet").CombinedOutput()
	if err != nil {
		msg := strings.TrimSpace(string(out))
		if msg == "" {
			msg = err.Error()
		}
		fmt.Fprintf(stderr, "gongoozler: %s not locked (%s); this run resolves its dependencies afresh\n", filepath.Base(file), msg)
		return
	}
	fmt.Fprintf(stderr, "gongoozler: locked %s on its first run: keep it beside the file, so every run of it gets the same packages\n", filepath.Base(lock))
}

func runCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("run", "[flags] FILE [ARGS...]", stderr)
	asJSON := fs.Bool("json", false, "print every event as JSON, one per line, on stdout")
	plain := fs.Bool("plain", false, "print plain lines, never redrawn (the default when stderr is not a terminal)")
	detach := fs.Bool("detach", false, "with a remote run: submit it, print its ID, and return")
	params := paramFlag{}
	fs.Var(params, "param", "set a parameter the file declares (repeatable): NAME=VALUE, the value as JSON or a string")
	paramsJSON := fs.String("params-json", "", "every parameter at once, as a JSON object (for boxes)")
	id := fs.String("id", "", "the run's ID (for boxes, which use the server's)")
	var ro runOptions
	ro.register(fs)
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if fs.NArg() == 0 {
		fs.Usage()
		return 2
	}
	if !ro.remote {
		placeSelf(ro.cgroup) // first, so nothing below is said twice
	}
	if *paramsJSON != "" {
		var all map[string]any
		d := json.NewDecoder(strings.NewReader(*paramsJSON))
		d.UseNumber()
		if err := d.Decode(&all); err != nil {
			return failf(stderr, 2, "--params-json is not a JSON object: %v", err)
		}
		for k, v := range all {
			if _, set := params[k]; !set {
				params[k] = v
			}
		}
	}
	p, err := ro.prepare(fs.Arg(0), stderr)
	if err != nil {
		return failf(stderr, 2, "%v", err)
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	supervise.Settle(state, "/proc")
	m := mode(*asJSON, *plain)
	if p.why != "" {
		fmt.Fprintf(stderr, "gongoozler: runs %s: %s\n", where(p.placement), p.why)
	}
	if p.placement == "remote" {
		return remoteRun(p, fs.Args()[1:], params, m, *detach, state, stdout, stderr, "")
	}
	res, code := localRun(context.Background(), p, fs.Args()[1:], params, m, ro.cgroup, state, *id, "", stdout, stderr)
	if res.ID != "" && m != term.JSON {
		fmt.Fprintf(stderr, "events: %s\n", filepath.Join(res.Dir, "events.jsonl"))
	}
	return code
}

func where(placement string) string {
	if placement == "remote" {
		return "on the server"
	}
	return "here"
}

// localRun runs a planned file here, under the supervisor, shown in mode m.
func localRun(ctx context.Context, p *plan, args []string, params map[string]any, m term.Mode, cgroup, state, id, sweepID string, stdout, stderr io.Writer) (supervise.Result, int) {
	o := supervise.Options{
		File: p.file, Args: args, Params: params, State: state, ID: id, Launcher: p.launcher, Signals: true,
		Memory: p.settings.Memory, CPUs: p.settings.CPUs, Timeout: p.settings.Timeout, Cgroup: cgroup,
		Version: version.Version, Placement: "local", Sweep: sweepID,
	}
	o.Render = renderer(m, stdout, stderr)
	res, err := supervise.Run(ctx, o)
	if err != nil {
		failf(stderr, 1, "%v", err)
		if res.ID == "" {
			return res, 1
		}
	}
	return res, res.Code()
}

// settle marks lost runs as such before a command lists or starts runs.
func settle(state string) {
	supervise.Settle(state, "/proc")
}

// waitEnded waits until a run's meta says it is no longer running, or the timeout passes.
func waitEnded(state, id string, timeout time.Duration) (runs.Meta, bool) {
	deadline := time.Now().Add(timeout)
	for {
		m, err := runs.ReadMeta(state, id)
		if err == nil && runs.Live(m) != "running" {
			return m, true
		}
		if time.Now().After(deadline) {
			return m, false
		}
		time.Sleep(25 * time.Millisecond)
	}
}
