// Package term shows a run as it goes, in one of three modes:
//   - Live, for a person at a terminal: the run's output scrolls, and a status block below it is
//     redrawn in place, at most ten times a second;
//   - Plain, for agents and logs: one line per change worth reading, nothing redrawn;
//   - JSON: every event, one per line, on stdout.
//
// A Renderer runs in a goroutine of its own. Event and Output queue and return at once, so
// storing a run's events never waits on a terminal. A terminal that cannot keep up loses lines
// from the display only (they are all in output.log), and the renderer says how many.
package term

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"math"
	"sort"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode/utf8"
	"unsafe"

	"github.com/cottrellashley/gongoozler/internal/events"
)

// Mode is how a run is shown.
type Mode int

const (
	Live Mode = iota
	Plain
	JSON
)

const queueCap = 50_000

type item struct {
	e            events.Event
	stream, line string
	isLine       bool
}

// Renderer shows one run (or a sweep's runs, through Sweep).
type Renderer struct {
	mode   Mode
	out    io.Writer // the run's stdout, and JSON events
	status io.Writer // the status, and the run's stderr
	width  func() int
	start  time.Time

	mu     sync.Mutex
	queue  []item
	hidden int
	notify chan struct{}
	stopc  chan struct{}
	done   chan struct{}

	stages   []string
	progress map[string]events.Event
	keys     []string
	metrics  map[string]events.Event
	resource events.Event
	hello    bool
	drawn    int
	dirty    bool
	printed  map[string]printed
	lastRes  time.Time
	json     *bufio.Writer
}

type printed struct {
	pct float64
	at  time.Time
}

// New is a renderer writing the run's stdout and JSON to out, and everything else to status.
// width reports the status terminal's width (Live only; nil means 100 columns).
func New(mode Mode, out, status io.Writer, width func() int) *Renderer {
	if width == nil {
		width = func() int { return 100 }
	}
	r := &Renderer{
		mode: mode, out: out, status: status, width: width, start: time.Now(),
		progress: map[string]events.Event{}, metrics: map[string]events.Event{}, printed: map[string]printed{},
		notify: make(chan struct{}, 1), stopc: make(chan struct{}), done: make(chan struct{}),
	}
	if mode == JSON {
		r.json = bufio.NewWriterSize(out, 1<<16)
	}
	go r.loop()
	return r
}

// Event queues an event (after it has been numbered and stored).
func (r *Renderer) Event(e events.Event) { r.push(item{e: e}) }

// Output queues a line the run printed. In JSON mode output travels as events instead.
func (r *Renderer) Output(stream, line string) {
	if r.mode == JSON {
		return
	}
	r.push(item{stream: stream, line: line, isLine: true})
}

func coalescable(it item) bool {
	if it.isLine {
		return true
	}
	switch it.e.Kind() {
	case "progress", "metric", "resource", "output":
		return true
	}
	return false
}

func (r *Renderer) push(it item) {
	r.mu.Lock()
	if len(r.queue) >= queueCap && coalescable(it) {
		r.hidden++
	} else {
		r.queue = append(r.queue, it)
	}
	r.mu.Unlock()
	select {
	case r.notify <- struct{}{}:
	default:
	}
}

// Close shows what is queued, stops, and clears the live status block; the lines printed above
// it, ending with the run's summary, stay.
func (r *Renderer) Close() {
	close(r.stopc)
	<-r.done
}

func (r *Renderer) loop() {
	defer close(r.done)
	tick := time.NewTicker(100 * time.Millisecond)
	defer tick.Stop()
	for {
		select {
		case <-r.notify:
			if r.mode != Live {
				r.process()
			}
		case <-tick.C:
			r.process()
			if r.mode == Live && r.dirty {
				r.erase()
				r.draw()
			}
			if r.json != nil {
				r.json.Flush()
			}
		case <-r.stopc:
			r.process()
			if r.mode == Live {
				r.erase()
			}
			if r.json != nil {
				r.json.Flush()
			}
			return
		}
	}
}

func (r *Renderer) process() {
	r.mu.Lock()
	items, hidden := r.queue, r.hidden
	r.queue, r.hidden = nil, 0
	r.mu.Unlock()
	if len(items) == 0 && hidden == 0 {
		return
	}
	var batch []line
	for _, it := range items {
		if it.isLine {
			w := r.out
			if it.stream == "stderr" {
				w = r.status
			}
			batch = append(batch, line{w, it.line})
			continue
		}
		if r.mode == JSON {
			if b, err := json.Marshal(it.e); err == nil {
				r.json.Write(append(b, '\n'))
			}
			continue
		}
		batch = append(batch, r.lines(it.e)...)
	}
	if hidden > 0 {
		batch = append(batch, line{r.status, fmt.Sprintf("… %d lines not shown: the terminal could not keep up (all are stored, and in output.log)", hidden)})
	}
	if r.mode == JSON {
		return
	}
	if r.mode == Live && len(batch) > 0 {
		r.erase()
	}
	for _, l := range batch {
		if r.mode == Plain && l.w == r.status {
			fmt.Fprintf(l.w, "[%7s] %s\n", Duration(time.Since(r.start).Seconds()), l.text)
		} else {
			fmt.Fprintln(l.w, l.text)
		}
	}
	if r.mode == Live && len(batch) > 0 {
		r.draw()
	}
}

type line struct {
	w    io.Writer
	text string
}

// lines turns an event into what to print above the block (and updates the block's state).
func (r *Renderer) lines(e events.Event) []line {
	r.dirty = true
	say := func(s string) []line { return []line{{r.status, s}} }
	switch e.Kind() {
	case "run.start":
		return say(fmt.Sprintf("run %s: %s", e.Str("id"), e.Str("file")))
	case "hello":
		r.hello = true
	case "stage":
		d, _ := e.Int("depth")
		depth := int(max(d, 0))
		pad := strings.Repeat("  ", depth)
		if e.Str("phase") == "begin" {
			r.stages = append(r.stages[:min(depth, len(r.stages))], e.Str("name"))
			return say(pad + "▸ " + e.Str("name"))
		}
		r.stages = r.stages[:min(depth, len(r.stages))]
		elapsed, _ := e.Num("elapsed")
		if ok, _ := e.Bool("ok"); ok {
			return say(fmt.Sprintf("%s✓ %s (%s)", pad, e.Str("name"), Duration(elapsed)))
		}
		return say(fmt.Sprintf("%s✗ %s (%s) %s", pad, e.Str("name"), Duration(elapsed), e.Str("error")))
	case "progress":
		key := e.Str("key")
		if _, seen := r.progress[key]; !seen {
			r.keys = append(r.keys, key)
		}
		r.progress[key] = e
		if r.mode == Plain {
			return r.plainProgress(key, e)
		}
	case "metric":
		r.metrics[e.Str("name")] = e
	case "resource":
		r.resource = e
		if r.mode == Plain && time.Since(r.lastRes) >= 30*time.Second {
			r.lastRes = time.Now()
			return say(Memory(e))
		}
	case "log":
		return say(fmt.Sprintf("[%s] %s", e.Str("level"), e.Str("msg")))
	case "error":
		where := e.Str("where")
		if th := e.Str("thread"); th != "" {
			where += ", thread " + th
		}
		return say(fmt.Sprintf("error %s: %s (%s)", e.Str("type"), e.Str("msg"), where))
	case "artifact":
		if msg := e.Str("error"); msg != "" {
			return say(fmt.Sprintf("artifact %s: not stored: %s", e.Str("name"), msg))
		}
		size, _ := e.Num("bytes")
		sum := e.Str("sha256")
		if len(sum) > 12 {
			sum = sum[:12]
		}
		return say(fmt.Sprintf("artifact %s (%s, sha256 %s)", e.Str("name"), Bytes(size), sum))
	case "params":
		b, _ := json.Marshal(e["values"])
		return say("params " + string(b))
	case "cli.protocol", "cli.version":
		return say(strings.TrimPrefix(e.Kind(), "cli.") + ": " + e.Str("error"))
	case "job.queued":
		return say("queued on the server as " + e.Str("id"))
	case "job.leased":
		n, _ := e.Int("attempt")
		if n > 1 {
			return say(fmt.Sprintf("running on worker %s (attempt %d)", e.Str("worker"), n))
		}
		return say("running on worker " + e.Str("worker"))
	case "job.expired":
		return say("worker " + e.Str("worker") + " went quiet")
	case "job.released":
		return say("worker " + e.Str("worker") + " gave the run back: " + e.Str("reason"))
	case "job.requeued":
		return say("queued again: " + e.Str("reason"))
	case "job.cancel":
		return say("cancelling: the server has told worker " + e.Str("worker"))
	case "run.end":
		return say(Summary(e))
	}
	return nil
}

func (r *Renderer) plainProgress(key string, e events.Event) []line {
	pct := percent(e)
	last, seen := r.printed[key]
	finished := !math.IsNaN(pct) && pct >= 100
	if seen && !finished && time.Since(last.at) < 10*time.Second && (math.IsNaN(pct) || pct-last.pct < 5) {
		return nil
	}
	if seen && finished && last.pct >= 100 {
		return nil
	}
	r.printed[key] = printed{pct: pct, at: time.Now()}
	return []line{{r.status, Progress(e)}}
}

func (r *Renderer) block() []string {
	var out []string
	if !r.hello {
		out = append(out, "starting: preparing the run's environment")
	}
	if len(r.stages) > 0 {
		out = append(out, "▸ "+strings.Join(r.stages, " › "))
	}
	for i, k := range r.keys {
		if i == 3 {
			out = append(out, fmt.Sprintf("… and %d more", len(r.keys)-3))
			break
		}
		out = append(out, Progress(r.progress[k]))
	}
	if len(r.metrics) > 0 {
		names := make([]string, 0, len(r.metrics))
		for n := range r.metrics {
			names = append(names, n)
		}
		sort.Strings(names)
		var parts []string
		for _, n := range names[:min(len(names), 4)] {
			if v, ok := r.metrics[n].Num("value"); ok {
				parts = append(parts, fmt.Sprintf("%s %s", n, Number(v)))
			}
		}
		if len(parts) > 0 {
			out = append(out, strings.Join(parts, "  "))
		}
	}
	tail := Duration(time.Since(r.start).Seconds())
	if r.resource != nil {
		tail = Memory(r.resource) + "  ·  " + tail
	}
	return append(out, tail)
}

func (r *Renderer) draw() {
	w := max(r.width()-1, 20)
	block := r.block()
	for _, l := range block {
		fmt.Fprintln(r.status, truncate(l, w))
	}
	r.drawn = len(block)
	r.dirty = false
}

func (r *Renderer) erase() {
	for range r.drawn {
		io.WriteString(r.status, "\x1b[1A\x1b[2K")
	}
	r.drawn = 0
}

func truncate(s string, width int) string {
	if utf8.RuneCountInString(s) <= width {
		return s
	}
	rs := []rune(s)
	return string(rs[:width-1]) + "…"
}

// ------------------------------------------------------------------------------- formatting

// Progress is one line for a progress event: `simulation  42.1%  2024-03-01T10:00:00Z  1.2M/s  eta 12s`.
func Progress(e events.Event) string {
	parts := []string{e.Str("key")}
	if p := percent(e); !math.IsNaN(p) {
		parts = append(parts, fmt.Sprintf("%5.1f%%", p))
	} else if d, ok := e.Num("done"); ok {
		s := Number(d)
		if u := e.Str("unit"); u != "" && u != "ns" {
			s += " " + u
		}
		parts = append(parts, s)
	}
	if sim, ok := e.Int("sim"); ok {
		parts = append(parts, time.Unix(0, sim).UTC().Format("2006-01-02T15:04:05Z"))
	}
	if rate, ok := e.Num("rate"); ok {
		parts = append(parts, Number(rate)+"/s")
	}
	if eta, ok := e.Num("eta"); ok {
		parts = append(parts, "eta "+Duration(eta))
	}
	return strings.Join(parts, "  ")
}

func percent(e events.Event) float64 {
	d, ok1 := e.Num("done")
	t, ok2 := e.Num("total")
	if !ok1 || !ok2 || t == 0 {
		return math.NaN()
	}
	return 100 * d / t
}

// Memory is `memory 1.2 GiB (peak 1.4 GiB, 3.1 GiB free, cgroup)` for a resource event.
func Memory(e events.Event) string {
	cur, _ := e.Num("memory")
	peak, _ := e.Num("peak")
	s := fmt.Sprintf("memory %s (peak %s", Bytes(cur), Bytes(peak))
	if avail, ok := e.Num("available"); ok {
		s += ", " + Bytes(avail) + " free"
	}
	if src := e.Str("source"); src != "" {
		s += ", " + src
	}
	return s + ")"
}

// Summary is the closing line for a run.end event.
func Summary(e events.Event) string {
	wall, _ := e.Num("wall")
	peak, _ := e.Num("peak")
	s := fmt.Sprintf("run %s %s in %s, memory peak %s", e.Str("id"), e.Str("status"), Duration(wall), Bytes(peak))
	if src := e.Str("memory_source"); src != "" {
		s += " (" + src + ")"
	}
	if code, ok := e.Int("exit_code"); ok && code != 0 {
		s += fmt.Sprintf(", exit code %d", code)
	}
	if sig := e.Str("signal"); sig != "" {
		s += ", " + sig
	}
	if note := e.Str("note"); note != "" {
		s += ": " + note
	}
	return s
}

// Bytes is a size in binary units: 512 B, 1.5 KiB, 3.2 GiB.
func Bytes(b float64) string {
	units := []string{"B", "KiB", "MiB", "GiB", "TiB"}
	i := 0
	for b >= 1024 && i < len(units)-1 {
		b /= 1024
		i++
	}
	if i == 0 {
		return fmt.Sprintf("%.0f B", b)
	}
	return fmt.Sprintf("%.1f %s", b, units[i])
}

// Duration is seconds as `0.4s`, `12.3s`, `4m05s` or `2h03m`.
func Duration(s float64) string {
	switch {
	case s < 60:
		return fmt.Sprintf("%.1fs", s)
	case s < 3600:
		return fmt.Sprintf("%dm%02ds", int(s)/60, int(s)%60)
	default:
		return fmt.Sprintf("%dh%02dm", int(s)/3600, int(s)%3600/60)
	}
}

// Number is a count or rate, short: 950, 12.3k, 4.5M.
func Number(v float64) string {
	a := math.Abs(v)
	switch {
	case a >= 1e9:
		return fmt.Sprintf("%.1fG", v/1e9)
	case a >= 1e6:
		return fmt.Sprintf("%.1fM", v/1e6)
	case a >= 1e4:
		return fmt.Sprintf("%.1fk", v/1e3)
	case a == math.Trunc(a):
		return fmt.Sprintf("%.0f", v)
	default:
		return fmt.Sprintf("%.4g", v)
	}
}

// IsTerminal reports whether fd is a terminal.
func IsTerminal(fd uintptr) bool {
	var t syscall.Termios
	_, _, errno := syscall.Syscall(syscall.SYS_IOCTL, fd, syscall.TCGETS, uintptr(unsafe.Pointer(&t)))
	return errno == 0
}

// Width is the terminal's width in columns, or 100.
func Width(fd uintptr) int {
	var ws struct{ Row, Col, X, Y uint16 }
	_, _, errno := syscall.Syscall(syscall.SYS_IOCTL, fd, syscall.TIOCGWINSZ, uintptr(unsafe.Pointer(&ws)))
	if errno != 0 || ws.Col == 0 {
		return 100
	}
	return int(ws.Col)
}
