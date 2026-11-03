package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"syscall"
	"text/tabwriter"
	"time"

	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/procmem"
	"github.com/cottrellashley/gongoozler/internal/runs"
	"github.com/cottrellashley/gongoozler/internal/supervise"
	"github.com/cottrellashley/gongoozler/internal/term"
)

// ------------------------------------------------------------------------------- ls

func lsCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("ls", "[--json] [--remote [--server URL]]", stderr)
	asJSON := fs.Bool("json", false, "print the runs' records as JSON")
	onServer := fs.Bool("remote", false, "list the server's runs instead of this machine's")
	serverURL := fs.String("server", os.Getenv("GONGOOZLER_SERVER"), "with --remote: the server's URL (default $GONGOOZLER_SERVER)")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	var all []runs.Meta
	var err error
	if *onServer {
		if *serverURL == "" {
			return failf(stderr, 2, "which server? --server URL or $GONGOOZLER_SERVER")
		}
		c := clientFor(runs.Meta{Server: *serverURL})
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		all, err = c.Runs(ctx)
	} else {
		settle(state)
		all, err = runs.List(state)
		refreshRemote(state, all)
	}
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	if *asJSON {
		if all == nil {
			all = []runs.Meta{}
		}
		printJSON(stdout, all)
		return 0
	}
	w := tabwriter.NewWriter(stdout, 0, 0, 2, ' ', 0)
	fmt.Fprintln(w, "RUN\tSTATUS\tWHERE\tWALL\tPEAK\tFILE")
	for _, m := range all {
		where := m.Placement
		if where == "" {
			where = "local"
		}
		fmt.Fprintf(w, "%s\t%s\t%s\t%s\t%s\t%s\n", m.ID, m.Status, where, term.Duration(m.Wall), term.Bytes(float64(m.Peak)), filepath.Base(m.File))
	}
	w.Flush()
	return 0
}

// refreshRemote asks the server how the remote runs it lists as going are doing, briefly, and
// records the answer.
func refreshRemote(state string, all []runs.Meta) {
	for i, m := range all {
		if m.Placement != "remote" || (m.Status != "running" && m.Status != "queued") {
			continue
		}
		c := clientFor(m)
		if c == nil {
			continue
		}
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		remote, err := c.Run(ctx, m.ID)
		cancel()
		if err == nil && remote.Status != m.Status {
			m.Status, m.ExitCode, m.Wall, m.Peak, m.Note, m.Ended = remote.Status, remote.ExitCode, remote.Wall, remote.Peak, remote.Note, remote.Ended
			_ = runs.WriteMeta(state, m) // what the server said, kept for later; the next ls asks again
			all[i] = m
		}
	}
}

// ------------------------------------------------------------------------------- events, attach

func eventsCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("events", "[--follow] RUN", stderr)
	follow := fs.Bool("follow", false, "keep printing events as the run makes them, until it ends")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if fs.NArg() != 1 {
		fs.Usage()
		return 2
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	m, err := resolve(state, fs.Arg(0))
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	w := bufio.NewWriterSize(stdout, 1<<16)
	defer w.Flush()
	print := func(e events.Event) error {
		b, err := json.Marshal(e)
		if err != nil {
			return err
		}
		w.Write(append(b, '\n'))
		if *follow {
			w.Flush()
		}
		return nil
	}
	if m.Placement == "remote" && (m.Status == "running" || m.Status == "queued") {
		// A remote run that has ended has all its events here; one still going is asked about.
		c := clientFor(m)
		if c == nil {
			return failf(stderr, 1, "run %s is on %s, and no server is set", m.ID, m.Server)
		}
		if !*follow {
			err = c.Events(context.Background(), m.ID, 0, print)
		} else {
			ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
			defer stop()
			_, err = c.Follow(ctx, m.ID, 0, print)
			if ctx.Err() != nil {
				err = nil
			}
		}
	} else {
		ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
		defer stop()
		err = tailLog(ctx, state, m.ID, *follow, print)
	}
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	return 0
}

// tailLog calls fn for each event in a local run's log; with follow, also for each one stored
// after, until the run ends (its run.end is read, or its supervisor is gone) or ctx is done.
func tailLog(ctx context.Context, state, id string, follow bool, fn func(events.Event) error) error {
	f, err := os.Open(filepath.Join(runs.Dir(state, id), "events.jsonl"))
	if err != nil {
		return err
	}
	defer f.Close()
	r := bufio.NewReaderSize(f, 1<<16)
	var partial []byte
	ended := false
	for {
		chunk, rerr := r.ReadBytes('\n')
		partial = append(partial, chunk...)
		if rerr == nil {
			line := bytes.TrimSuffix(partial, []byte("\n"))
			partial = nil
			d := json.NewDecoder(bytes.NewReader(line))
			d.UseNumber()
			var e events.Event
			if d.Decode(&e) == nil {
				if err := fn(e); err != nil {
					return err
				}
				if e.Kind() == "run.end" {
					ended = true
				}
			}
			continue
		}
		if rerr != io.EOF {
			return rerr
		}
		if !follow || ended {
			return nil
		}
		m, err := runs.ReadMeta(state, id)
		if err == nil && runs.Live(m) != "running" {
			// The run is over: read what was stored before its supervisor finished, then stop.
			follow = false
			continue
		}
		select {
		case <-ctx.Done():
			return nil
		case <-time.After(50 * time.Millisecond):
		}
	}
}

func attachCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("attach", "[--plain | --json] RUN", stderr)
	asJSON := fs.Bool("json", false, "print every event as JSON, one per line, on stdout")
	plain := fs.Bool("plain", false, "print plain lines, never redrawn")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if fs.NArg() != 1 {
		fs.Usage()
		return 2
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	settle(state)
	m, err := resolve(state, fs.Arg(0))
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	md := mode(*asJSON, *plain)
	if m.Placement == "remote" {
		c := clientFor(m)
		if c == nil {
			return failf(stderr, 1, "run %s is on %s, and no server is set", m.ID, m.Server)
		}
		return watchRemote(c, state, m.ID, md, false, stdout, stderr)
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP)
	defer stop()
	r := renderer(md, stdout, stderr)
	err = tailLog(ctx, state, m.ID, true, func(e events.Event) error { show(r, md, e); return nil })
	r.Close()
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	if ctx.Err() != nil {
		fmt.Fprintf(stderr, "gongoozler: stopped watching %s; it goes on\n", m.ID)
		return 0
	}
	final, _ := runs.ReadMeta(state, m.ID)
	return final.ExitCode
}

// ------------------------------------------------------------------------------- kill

func killCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("kill", "[--force] RUN", stderr)
	force := fs.Bool("force", false, "end every process of the run at once, and its supervisor too if it does not answer")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if fs.NArg() != 1 {
		fs.Usage()
		return 2
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	m, err := resolve(state, fs.Arg(0))
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	if m.Placement == "remote" {
		c := clientFor(m)
		if c == nil {
			return failf(stderr, 1, "run %s is on %s, and no server is set", m.ID, m.Server)
		}
		rm, err := c.Cancel(context.Background(), m.ID)
		if err != nil {
			return failf(stderr, 1, "%v", err)
		}
		fmt.Fprintf(stdout, "%s: cancelling on the server (%s)\n", m.ID, rm.Status)
		return 0
	}
	switch m.Status {
	case "lost":
		for _, s := range supervise.Settle(state, "/proc") {
			if s.ID == m.ID {
				fmt.Fprintf(stdout, "%s: lost; %s\n", m.ID, s.Note)
				return 0
			}
		}
		fmt.Fprintf(stdout, "%s: lost; nothing of it is running\n", m.ID)
		return 0
	case "running":
	default:
		fmt.Fprintf(stdout, "%s: already ended (%s)\n", m.ID, m.Status)
		return 0
	}
	sup := m.SupervisorPID
	syscall.Kill(sup, syscall.SIGUSR1)
	wait := 10 * time.Second
	if *force {
		time.Sleep(50 * time.Millisecond)
		syscall.Kill(sup, syscall.SIGUSR1) // a second: every process of the run, at once
		wait = 3 * time.Second
	}
	final, ended := waitEnded(state, m.ID, wait)
	if !ended && *force && procmem.Alive("/proc", sup, m.SupervisorStart) {
		syscall.Kill(sup, syscall.SIGKILL)
		time.Sleep(100 * time.Millisecond)
		supervise.Settle(state, "/proc")
		final, ended = waitEnded(state, m.ID, time.Second)
	}
	if !ended {
		return failf(stderr, 1, "%s is still stopping (gongoozler kill --force %s ends it at once)", m.ID, m.ID)
	}
	fmt.Fprintf(stdout, "%s: %s\n", m.ID, runs.Live(final))
	return 0
}

// ------------------------------------------------------------------------------- diff

// comparison is how two runs differ: in what went in, and in what they kept.
type comparison struct {
	inputs  []string
	same    []string
	differs []string
	onlyA   []string
	onlyB   []string
}

func (c comparison) reproduced() bool {
	return len(c.differs) == 0 && len(c.onlyA) == 0 && len(c.onlyB) == 0 && len(c.same) > 0
}

func compareRuns(state string, a, b runs.Meta) comparison {
	var c comparison
	short := func(s string) string {
		if len(s) > 12 {
			return s[:12]
		}
		if s == "" {
			return "none"
		}
		return s
	}
	if a.SHA256 != b.SHA256 {
		c.inputs = append(c.inputs, fmt.Sprintf("file: %s → %s", short(a.SHA256), short(b.SHA256)))
	}
	if a.Lock != b.Lock {
		c.inputs = append(c.inputs, fmt.Sprintf("lock: %s → %s", short(a.Lock), short(b.Lock)))
	}
	keys := map[string]bool{}
	for k := range a.Params {
		keys[k] = true
	}
	for k := range b.Params {
		keys[k] = true
	}
	var names []string
	for k := range keys {
		names = append(names, k)
	}
	sort.Strings(names)
	for _, k := range names {
		va, oka := a.Params[k]
		vb, okb := b.Params[k]
		if oka != okb || !reflect.DeepEqual(fmt.Sprint(va), fmt.Sprint(vb)) {
			c.inputs = append(c.inputs, fmt.Sprintf("param %s: %v → %v", k, show2(va, oka), show2(vb, okb)))
		}
	}
	if strings.Join(a.Args, " ") != strings.Join(b.Args, " ") {
		c.inputs = append(c.inputs, fmt.Sprintf("args: %q → %q", a.Args, b.Args))
	}
	if a.Version != b.Version {
		c.inputs = append(c.inputs, fmt.Sprintf("gongoozler: %s → %s", a.Version, b.Version))
	}
	da, db := filepath.Join(runs.Dir(state, a.ID), "artifacts"), filepath.Join(runs.Dir(state, b.ID), "artifacts")
	fa, fb := artifactSums(da), artifactSums(db)
	var all []string
	for k := range fa {
		all = append(all, k)
	}
	for k := range fb {
		if _, ok := fa[k]; !ok {
			all = append(all, k)
		}
	}
	sort.Strings(all)
	for _, name := range all {
		sa, oka := fa[name]
		sb, okb := fb[name]
		switch {
		case !okb:
			c.onlyA = append(c.onlyA, name)
		case !oka:
			c.onlyB = append(c.onlyB, name)
		case sa == sb:
			c.same = append(c.same, name)
		default:
			c.differs = append(c.differs, name+": "+firstDifference(filepath.Join(da, name), filepath.Join(db, name)))
		}
	}
	return c
}

func show2(v any, ok bool) string {
	if !ok {
		return "(unset)"
	}
	b, _ := json.Marshal(v)
	return string(b)
}

func artifactSums(dir string) map[string]string {
	out := map[string]string{}
	entries, _ := os.ReadDir(dir)
	for _, d := range entries {
		if !d.Type().IsRegular() || strings.HasSuffix(d.Name(), ".partial") {
			continue
		}
		if sum, err := sha256File(filepath.Join(dir, d.Name())); err == nil {
			out[d.Name()] = sum
		}
	}
	return out
}

func sha256File(path string) (string, error) {
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

// firstDifference says where two files part: the first line that differs, and for JSON objects
// the fields that do.
func firstDifference(a, b string) string {
	ba, _ := os.ReadFile(a)
	bb, _ := os.ReadFile(b)
	la, lb := strings.Split(string(ba), "\n"), strings.Split(string(bb), "\n")
	for i := 0; i < max(len(la), len(lb)); i++ {
		var x, y string
		if i < len(la) {
			x = la[i]
		}
		if i < len(lb) {
			y = lb[i]
		}
		if x == y {
			continue
		}
		where := fmt.Sprintf("line %d", i+1)
		if len(la) != len(lb) {
			where += fmt.Sprintf(" (%d lines → %d)", len(la)-1, len(lb)-1)
		}
		var ox, oy map[string]any
		if json.Unmarshal([]byte(x), &ox) == nil && json.Unmarshal([]byte(y), &oy) == nil {
			if fields := differingFields("", ox, oy); len(fields) > 0 {
				if len(fields) > 5 {
					fields = append(fields[:5], "…")
				}
				where += ": " + strings.Join(fields, ", ")
			}
		}
		return where
	}
	return "differs"
}

func differingFields(prefix string, a, b map[string]any) []string {
	keys := map[string]bool{}
	for k := range a {
		keys[k] = true
	}
	for k := range b {
		keys[k] = true
	}
	var names []string
	for k := range keys {
		names = append(names, k)
	}
	sort.Strings(names)
	var out []string
	for _, k := range names {
		ma, oka := a[k].(map[string]any)
		mb, okb := b[k].(map[string]any)
		if oka && okb {
			out = append(out, differingFields(prefix+k+".", ma, mb)...)
			continue
		}
		if !reflect.DeepEqual(a[k], b[k]) {
			out = append(out, prefix+k)
		}
	}
	return out
}

func (c comparison) print(w io.Writer, a, b runs.Meta) {
	fmt.Fprintf(w, "%s → %s\n", a.ID, b.ID)
	if len(c.inputs) == 0 {
		fmt.Fprintln(w, "inputs: the same file, lock, parameters and arguments")
	} else {
		fmt.Fprintln(w, "inputs:")
		for _, s := range c.inputs {
			fmt.Fprintln(w, "  "+s)
		}
	}
	fmt.Fprintf(w, "results: %d the same, %d differ, %d only in the first, %d only in the second\n", len(c.same), len(c.differs), len(c.onlyA), len(c.onlyB))
	for _, s := range c.differs {
		fmt.Fprintln(w, "  differs: "+s)
	}
	for _, s := range c.onlyA {
		fmt.Fprintln(w, "  only in the first: "+s)
	}
	for _, s := range c.onlyB {
		fmt.Fprintln(w, "  only in the second: "+s)
	}
}

func diffCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("diff", "RUN RUN", stderr)
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if fs.NArg() != 2 {
		fs.Usage()
		return 2
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	a, err := resolve(state, fs.Arg(0))
	if err != nil {
		return failf(stderr, 2, "%v", err)
	}
	b, err := resolve(state, fs.Arg(1))
	if err != nil {
		return failf(stderr, 2, "%v", err)
	}
	c := compareRuns(state, a, b)
	c.print(stdout, a, b)
	if c.reproduced() {
		return 0
	}
	return 1
}

// ------------------------------------------------------------------------------- rerun

func rerunCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("rerun", "[--json | --plain] RUN", stderr)
	asJSON := fs.Bool("json", false, "print every event as JSON, one per line, on stdout")
	plain := fs.Bool("plain", false, "print plain lines, never redrawn")
	launcher := fs.String("launcher", "", "the command to give the file to, instead of `uv run --script` (for tests)")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if fs.NArg() != 1 {
		fs.Usage()
		return 2
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	settle(state)
	a, err := resolve(state, fs.Arg(0))
	if err != nil {
		return failf(stderr, 2, "%v", err)
	}
	if a.Status == "running" || a.Status == "queued" {
		return failf(stderr, 2, "run %s has not ended", a.ID)
	}
	file, note, err := sourceOf(state, a)
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	if note != "" {
		fmt.Fprintf(stderr, "gongoozler: %s\n", note)
	}
	if a.Cwd != "" {
		if err := os.Chdir(a.Cwd); err != nil {
			fmt.Fprintf(stderr, "gongoozler: the run's working directory %s is gone; rerunning from here\n", a.Cwd)
		}
	}
	ro := runOptions{noLock: true, launcher: *launcher, cgroup: "auto"}
	if a.Placement == "remote" {
		ro.remote = true
		ro.server = a.Server
	} else {
		ro.local = true
	}
	p, err := ro.prepare(file, stderr)
	if err != nil {
		return failf(stderr, 2, "%v", err)
	}
	m := mode(*asJSON, *plain)
	var b runs.Meta
	if p.placement == "remote" {
		meta, err := submit(context.Background(), p, a.Args, a.Params, state, "")
		if err != nil {
			return failf(stderr, 1, "%v", err)
		}
		if code := watchRemote(p.client, state, meta.ID, m, true, stdout, stderr); code != 0 {
			fmt.Fprintf(stderr, "gongoozler: the rerun did not succeed (exit %d)\n", code)
		}
		b, _ = runs.ReadMeta(state, meta.ID)
	} else {
		res, _ := localRun(context.Background(), p, a.Args, a.Params, m, "auto", state, "", "", stdout, stderr)
		b = res.Meta
	}
	if b.ID == "" {
		return 1
	}
	c := compareRuns(state, a, b)
	out := stderr
	if m != term.JSON {
		out = stdout
	}
	c.print(out, a, b)
	if c.reproduced() {
		fmt.Fprintf(out, "reproduced: %d results byte for byte\n", len(c.same))
		return 0
	}
	fmt.Fprintln(out, "not reproduced")
	return 1
}

// sourceOf is the file to rerun a run from: the original, if it is unchanged, or else the copy kept
// with the run.
func sourceOf(state string, m runs.Meta) (string, string, error) {
	if sum, err := sha256File(m.File); err == nil && sum == m.SHA256 {
		lockOK := true
		if m.Lock != "" {
			ls, err := sha256File(m.File + ".lock")
			lockOK = err == nil && ls == m.Lock
		}
		if lockOK {
			return m.File, "", nil
		}
	}
	copy := filepath.Join(runs.Dir(state, m.ID), "source", filepath.Base(m.File))
	if sum, err := sha256File(copy); err == nil && sum == m.SHA256 {
		return copy, fmt.Sprintf("%s has changed since run %s; rerunning the copy kept with it", m.File, m.ID), nil
	}
	return "", "", fmt.Errorf("neither %s nor the copy kept with run %s is the file it ran", m.File, m.ID)
}
