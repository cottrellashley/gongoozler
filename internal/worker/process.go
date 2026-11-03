package worker

import (
	"context"
	"os"
	"path/filepath"
	"sort"
	"time"

	"github.com/cottrellashley/gongoozler/internal/api"
	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/runs"
	"github.com/cottrellashley/gongoozler/internal/supervise"
)

// Process runs jobs with the supervisor in the worker's own process: on the worker's machine, or in
// the worker's container. Each run gets the cgroup the supervisor can make there.
type Process struct {
	// Launcher is the command a file is given to. Default: uv run --script.
	Launcher []string
	Env      []string
	Cgroup   string
	Version  string
}

// Run runs a job under the supervisor, recording it in dir.
func (p Process) Run(ctx context.Context, job api.Job, dir string, emit func(events.Event)) (runs.Meta, []string, error) {
	o := supervise.Options{
		File: filepath.Join(dir, filepath.Base(job.Spec.File)), Args: job.Spec.Args, Params: job.Spec.Params,
		State: filepath.Join(dir, "state"), ID: job.ID, Launcher: p.Launcher, Env: p.Env, Cgroup: p.Cgroup,
		Memory: job.Spec.Memory, CPUs: job.Spec.CPUs, Timeout: time.Duration(job.Spec.Timeout * float64(time.Second)),
		OnEvent: emit, Version: p.Version, Placement: "box", Sweep: job.Spec.Sweep,
	}
	res, err := supervise.Run(ctx, o)
	return res.Meta, files(filepath.Join(res.Dir, "artifacts")), err
}

// files is the regular files in dir, by name.
func files(dir string) []string {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil
	}
	var out []string
	for _, d := range entries {
		if d.Type().IsRegular() {
			out = append(out, filepath.Join(dir, d.Name()))
		}
	}
	sort.Strings(out)
	return out
}
