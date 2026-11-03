package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"sync"
	"syscall"
	"text/tabwriter"
	"time"

	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/procmem"
	"github.com/cottrellashley/gongoozler/internal/runs"
	"github.com/cottrellashley/gongoozler/internal/supervise"
	"github.com/cottrellashley/gongoozler/internal/sweep"
	"github.com/cottrellashley/gongoozler/internal/term"
	"github.com/cottrellashley/gongoozler/internal/version"
)

// sweepRecord is what STATE/sweeps/ID.json keeps.
type sweepRecord struct {
	ID      string         `json:"id"`
	File    string         `json:"file"`
	Grid    sweep.Grid     `json:"grid"`
	Rank    string         `json:"rank,omitempty"`
	Started time.Time      `json:"started"`
	Ended   time.Time      `json:"ended"`
	Runs    []sweep.Entry  `json:"runs"`
	Where   string         `json:"where"`
	Fixed   map[string]any `json:"fixed,omitempty"`
}

func sweepCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("sweep", "[flags] FILE [ARGS...]", stderr)
	asJSON := fs.Bool("json", false, "print each run's end, and the sweep's, as JSON events on stdout")
	plain := fs.Bool("plain", false, "print plain lines, never redrawn")
	grid := sweepFlag{}
	fs.Var(grid, "param", "sweep a parameter (repeatable): NAME=V1,V2,... or NAME=[JSON values]; adds to, and overrides, the file's sweep")
	fixed := paramFlag{}
	fs.Var(fixed, "set", "hold a parameter at one value in every run (repeatable): NAME=VALUE")
	jobs := fs.Int("jobs", 0, "runs at once, here (default: what the CPUs and memory allow)")
	rank := fs.String("rank", "", "rank the runs by a statistic in their stats.json, such as 'pnls.USD.PnL (total)'; '-' first ranks lowest first")
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
	p, err := ro.prepare(fs.Arg(0), stderr)
	if err != nil {
		return failf(stderr, 2, "%v", err)
	}
	g := sweep.Grid{}
	for k, v := range p.settings.Sweep {
		g[k] = v
	}
	for k, v := range grid {
		g[k] = v
	}
	for k := range fixed {
		delete(g, k)
	}
	points := g.Points()
	if len(points) == 0 {
		return failf(stderr, 2, "nothing to sweep: --param NAME=V1,V2 or the file's [tool.gongoozler] sweep")
	}
	for _, pt := range points {
		for k, v := range fixed {
			pt[k] = v
		}
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	settle(state)
	id := "sweep-" + runs.NewID(time.Now())
	rec := sweepRecord{ID: id, File: p.file, Grid: g, Rank: *rank, Started: time.Now(), Where: p.placement, Fixed: fixed}
	m := mode(*asJSON, *plain)
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP)
	defer stop()
	view := newSweepView(m, len(points), stdout, stderr)
	fmt.Fprintf(stderr, "sweep %s: %d runs of %s, %s\n", id, len(points), filepath.Base(p.file), where(p.placement))
	var entries []sweep.Entry
	if p.placement == "remote" {
		entries = sweepRemote(ctx, p, fs.Args()[1:], points, state, id, view, stderr)
	} else {
		n := *jobs
		if n <= 0 {
			n = defaultJobs(p)
		}
		entries = sweepLocal(ctx, p, fs.Args()[1:], points, ro.cgroup, state, id, n, view)
	}
	view.close()
	if *rank != "" {
		path := sweep.Path(*rank)
		for i := range entries {
			if v, ok := statOf(state, entries[i].ID, path); ok {
				entries[i].Value = &v
			}
		}
		sweep.Rank(entries, *rank)
	}
	rec.Runs, rec.Ended = entries, time.Now()
	if err := writeSweep(state, rec); err != nil {
		failf(stderr, 1, "the sweep's record: %v", err)
	}
	failed := 0
	for _, e := range entries {
		if e.Status != "ok" {
			failed++
		}
	}
	if m == term.JSON {
		b, _ := json.Marshal(events.New("sweep.end", map[string]any{"id": id, "runs": entries, "failed": failed, "rank": *rank}))
		fmt.Fprintln(stdout, string(b))
	} else {
		printRanking(stdout, entries, *rank)
		fmt.Fprintf(stderr, "sweep %s: %d ok, %d not; %s\n", id, len(entries)-failed, failed, filepath.Join(state, "sweeps", id+".json"))
	}
	if ctx.Err() != nil {
		return 130
	}
	if failed > 0 {
		return 1
	}
	return 0
}

// defaultJobs is how many runs fit here at once: by the CPUs each declares (one when it declares
// none), and by the memory each declares against what is available.
func defaultJobs(p *plan) int {
	per := p.settings.CPUs
	if per <= 0 {
		per = 1
	}
	n := max(int(float64(runtime.NumCPU())/per), 1)
	if p.settings.Memory > 0 {
		if avail, err := procmem.Available("/proc"); err == nil {
			n = min(n, max(int(avail/p.settings.Memory), 1))
		}
	}
	return n
}

func sweepLocal(ctx context.Context, p *plan, args []string, points []map[string]any, cgroup, state, id string, jobs int, view *sweepView) []sweep.Entry {
	entries := make([]sweep.Entry, len(points))
	next := make(chan int)
	var wg sync.WaitGroup
	for w := 0; w < min(jobs, len(points)); w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range next {
				pt := points[i]
				view.started(i, pt)
				o := supervise.Options{
					File: p.file, Args: args, Params: pt, State: state, Launcher: p.launcher, Cgroup: cgroup,
					Memory: p.settings.Memory, CPUs: p.settings.CPUs, Timeout: p.settings.Timeout,
					Version: version.Version, Placement: "local", Sweep: id,
					OnEvent: func(e events.Event) { view.event(i, e) },
				}
				res, err := supervise.Run(ctx, o)
				status := res.Status
				if err != nil && status == "" {
					status = "failed"
				}
				entries[i] = sweep.Entry{ID: res.ID, Params: pt, Status: status}
				view.ended(i, res.Meta, err)
			}
		}()
	}
	for i := range points {
		if ctx.Err() != nil {
			entries[i] = sweep.Entry{Params: points[i], Status: "cancelled"}
			continue
		}
		select {
		case next <- i:
		case <-ctx.Done():
			entries[i] = sweep.Entry{Params: points[i], Status: "cancelled"}
		}
	}
	close(next)
	wg.Wait()
	return entries
}

func sweepRemote(ctx context.Context, p *plan, args []string, points []map[string]any, state, id string, view *sweepView, stderr io.Writer) []sweep.Entry {
	entries := make([]sweep.Entry, len(points))
	pending := map[int]string{}
	for i, pt := range points {
		m, err := submit(context.Background(), p, args, pt, state, id)
		if err != nil {
			entries[i] = sweep.Entry{Params: pt, Status: "failed"}
			failf(stderr, 1, "cannot submit %s: %v", sweep.Label(pt), err)
			continue
		}
		entries[i] = sweep.Entry{ID: m.ID, Params: pt, Status: "queued"}
		pending[i] = m.ID
		view.started(i, pt)
	}
	cancelled := false
	for len(pending) > 0 {
		if ctx.Err() != nil && !cancelled {
			cancelled = true
			for _, rid := range pending {
				if _, err := p.client.Cancel(context.Background(), rid); err != nil {
					failf(stderr, 1, "could not cancel %s: %v", rid, err)
				}
			}
		}
		for i, rid := range pending {
			m, err := p.client.Run(context.Background(), rid)
			if err != nil || m.Status == "queued" || m.Status == "running" {
				continue
			}
			m, _ = pullRun(context.Background(), p.client, state, rid, false)
			entries[i].Status = m.Status
			view.ended(i, m, nil)
			delete(pending, i)
		}
		time.Sleep(250 * time.Millisecond)
	}
	return entries
}

func statOf(state, id, path string) (float64, bool) {
	if id == "" {
		return 0, false
	}
	b, err := os.ReadFile(filepath.Join(runs.Dir(state, id), "artifacts", "stats.json"))
	if err != nil {
		return 0, false
	}
	var stats map[string]any
	if json.Unmarshal(b, &stats) != nil {
		return 0, false
	}
	return sweep.Stat(stats, path)
}

func writeSweep(state string, rec sweepRecord) error {
	dir := filepath.Join(state, "sweeps")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	b, err := json.MarshalIndent(rec, "", "  ")
	if err != nil {
		return err
	}
	tmp := filepath.Join(dir, rec.ID+".json.tmp")
	if err := os.WriteFile(tmp, append(b, '\n'), 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, filepath.Join(dir, rec.ID+".json"))
}

func printRanking(w io.Writer, entries []sweep.Entry, rank string) {
	tw := tabwriter.NewWriter(w, 0, 0, 2, ' ', 0)
	if rank != "" {
		fmt.Fprintf(tw, "#\t%s\tSTATUS\tRUN\tPARAMS\n", sweep.Path(rank))
	} else {
		fmt.Fprintf(tw, "#\tSTATUS\tRUN\tPARAMS\n")
	}
	for i, e := range entries {
		if rank != "" {
			v := "-"
			if e.Value != nil {
				v = term.Number(*e.Value)
			}
			fmt.Fprintf(tw, "%d\t%s\t%s\t%s\t%s\n", i+1, v, e.Status, e.ID, sweep.Label(e.Params))
		} else {
			fmt.Fprintf(tw, "%d\t%s\t%s\t%s\n", i+1, e.Status, e.ID, sweep.Label(e.Params))
		}
	}
	tw.Flush()
}

// sweepView shows a sweep: a line as each run ends and, on a terminal, a status line of what is
// running, redrawn in place; or, in JSON mode, each run's end as an event.
type sweepView struct {
	mode    term.Mode
	out     io.Writer
	status  io.Writer
	total   int
	mu      sync.Mutex
	done    int
	running map[int]string
	pct     map[int]float64
	stopc   chan struct{}
	stopped chan struct{}
	drawn   bool
}

func newSweepView(m term.Mode, total int, out, status io.Writer) *sweepView {
	v := &sweepView{mode: m, out: out, status: status, total: total, running: map[int]string{}, pct: map[int]float64{},
		stopc: make(chan struct{}), stopped: make(chan struct{})}
	go v.loop()
	return v
}

func (v *sweepView) started(i int, params map[string]any) {
	v.mu.Lock()
	v.running[i] = sweep.Label(params)
	v.mu.Unlock()
}

func (v *sweepView) event(i int, e events.Event) {
	if e.Kind() != "progress" {
		return
	}
	done, ok1 := e.Num("done")
	total, ok2 := e.Num("total")
	if ok1 && ok2 && total > 0 {
		v.mu.Lock()
		v.pct[i] = 100 * done / total
		v.mu.Unlock()
	}
}

func (v *sweepView) ended(i int, m runs.Meta, err error) {
	v.mu.Lock()
	defer v.mu.Unlock()
	label := v.running[i]
	delete(v.running, i)
	delete(v.pct, i)
	v.done++
	if v.mode == term.JSON {
		b, _ := json.Marshal(events.New("sweep.run", map[string]any{"run": m.ID, "status": m.Status, "exit_code": m.ExitCode, "wall": m.Wall, "params": m.Params}))
		fmt.Fprintln(v.out, string(b))
		return
	}
	v.erase()
	note := ""
	if err != nil {
		note = " " + err.Error()
	} else if m.Status != "ok" && m.Note != "" {
		note = " (" + m.Note + ")"
	}
	fmt.Fprintf(v.status, "[%d/%d] %-9s %s  %s  %s%s\n", v.done, v.total, m.Status, label, term.Duration(m.Wall), m.ID, note)
}

func (v *sweepView) erase() {
	if v.drawn {
		fmt.Fprint(v.status, "\r\x1b[K")
		v.drawn = false
	}
}

func (v *sweepView) loop() {
	defer close(v.stopped)
	if v.mode != term.Live {
		<-v.stopc
		return
	}
	t := time.NewTicker(200 * time.Millisecond)
	defer t.Stop()
	for {
		select {
		case <-v.stopc:
			v.mu.Lock()
			v.erase()
			v.mu.Unlock()
			return
		case <-t.C:
		}
		v.mu.Lock()
		line := fmt.Sprintf("%d/%d done, %d running", v.done, v.total, len(v.running))
		for i, label := range v.running {
			if pct, ok := v.pct[i]; ok {
				line += fmt.Sprintf(" · %s %.0f%%", label, pct)
			} else {
				line += " · " + label
			}
		}
		v.erase()
		fmt.Fprint(v.status, truncate(line, term.Width(os.Stderr.Fd())-1))
		v.drawn = true
		v.mu.Unlock()
	}
}

func (v *sweepView) close() {
	close(v.stopc)
	<-v.stopped
}

func truncate(s string, width int) string {
	r := []rune(s)
	if width > 1 && len(r) > width {
		return string(r[:width-1]) + "…"
	}
	return s
}
