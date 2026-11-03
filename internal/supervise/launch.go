package supervise

import (
	"context"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"os/exec"
	"os/signal"
	"strconv"
	"sync"
	"syscall"
	"time"

	"runtime"

	"github.com/cottrellashley/gongoozler/internal/procmem"
)

// process is the run's process tree, as the supervisor holds it.
type process struct {
	cmd    *exec.Cmd
	pid    int
	proc   string
	anchor *exec.Cmd // the sleeper that keeps the run's scope alive until its numbers are read
	cgroup *procmem.Cgroup
	cgfd   int

	waitDone chan struct{}
	waitErr  error
	released chan struct{}
	gone     chan struct{}
}

// protected holds the PIDs the reaper must never reap: each run's own child and its anchor, which
// os/exec waits for. A worker runs several runs in one process.
var protected sync.Map

var subreaperOnce sync.Once

// becomeSubreaper makes orphans the run leaves come back to the supervisor (not to init), so they
// can be found, stopped and reaped.
func becomeSubreaper() {
	subreaperOnce.Do(func() {
		const prSetChildSubreaper = 36
		syscall.RawSyscall(syscall.SYS_PRCTL, prSetChildSubreaper, 1, 0)
	})
}

// launch starts the run. One goroutine, locked to its OS thread, starts the children and outlives
// them: Linux sends a child's parent-death signal when the thread that started it ends, so the run
// dies with the supervisor (SIGTERM, which uv forwards), and never sooner.
func launch(o Options, id string, args, env []string, stdout, stderr io.Writer) (*process, error) {
	p := &process{proc: o.Proc, cgfd: -1, waitDone: make(chan struct{}), released: make(chan struct{}), gone: make(chan struct{})}
	started := make(chan error, 1)
	go func() {
		runtime.LockOSThread()
		defer runtime.UnlockOSThread()
		defer close(p.gone)
		err := p.start(o, id, args, env, stdout, stderr)
		started <- err
		if err != nil {
			close(p.waitDone)
			p.dropScope()
			return
		}
		p.waitErr = p.cmd.Wait()
		close(p.waitDone)
		<-p.released
		p.dropScope()
	}()
	if err := <-started; err != nil {
		<-p.gone
		return nil, err
	}
	return p, nil
}

func (p *process) start(o Options, id string, args, env []string, stdout, stderr io.Writer) error {
	switch {
	case o.Cgroup == "off":
	case procmem.ScopesAvailable():
		if err := p.makeScope(o, id); err != nil && o.Cgroup == "require" {
			return fmt.Errorf("the run requires a cgroup: %w", err)
		}
	case o.Cgroup == "require":
		return errors.New("the run requires a cgroup, and systemd user scopes are not available here")
	}
	if p.cgroup == nil && o.Cgroup != "off" {
		if cg, ok := procmem.SelfContained(o.Proc); ok {
			p.cgroup = &cg // a container: the box is the cgroup, shared with the supervisor
		}
	}
	build := func(intoScope bool) *exec.Cmd {
		cmd := exec.Command(o.Launcher[0], args...)
		cmd.Env, cmd.Stdout, cmd.Stderr = env, stdout, stderr
		cmd.WaitDelay = 2 * time.Second // a grandchild holding the output open cannot hold the supervisor
		attr := &syscall.SysProcAttr{Setpgid: true, Pdeathsig: syscall.SIGTERM}
		if intoScope {
			attr.UseCgroupFD, attr.CgroupFD = true, p.cgfd
		}
		cmd.SysProcAttr = attr
		return cmd
	}
	inScope := p.cgfd >= 0
	cmd := build(inScope)
	if err := cmd.Start(); err != nil {
		if !inScope {
			return err
		}
		// The kernel would not start it inside the scope (an older kernel, or this process's cgroup
		// is one it may not move children out of): run without one, and account by PSS.
		p.dropScope()
		if o.Cgroup == "require" {
			return fmt.Errorf("the run requires a cgroup, and it could not start in one: %w", err)
		}
		cmd = build(false)
		if err := cmd.Start(); err != nil {
			return err
		}
	}
	p.cmd, p.pid = cmd, cmd.Process.Pid
	protected.Store(p.pid, true)
	return nil
}

// switchScript is the scope's anchor. It holds the scope open until the supervisor has read the
// run's final numbers, and it is the run's dead man's switch: its parent-death signal is SIGTERM,
// so when the supervisor dies it kills every process in the scope, at once and whatever signals
// they ignore (cgroup.kill, Linux 5.14).
const switchScript = `trap 'echo 1 > "/sys/fs/cgroup$(sed -n "s/^0:://p" /proc/self/cgroup)/cgroup.kill"' TERM
while :; do sleep 3600 & wait $!; done`

// makeScope creates the run's systemd scope, anchored by switchScript. Memory and CPU limits are
// the scope's.
func (p *process) makeScope(o Options, id string) error {
	unit := "gongoozler-" + id
	// OOMPolicy=continue: by default systemd stops a unit when the kernel kills one of its processes
	// for memory, which would take the anchor, and the cgroup's numbers, with it.
	args := []string{"--user", "--scope", "--quiet", "--collect", "--unit", unit, "-p", "OOMPolicy=continue"}
	if o.Memory > 0 {
		args = append(args, "-p", "MemoryMax="+strconv.FormatInt(o.Memory, 10), "-p", "MemorySwapMax=0")
	}
	if o.CPUs > 0 {
		args = append(args, "-p", fmt.Sprintf("CPUQuota=%d%%", int(math.Ceil(o.CPUs*100))))
	}
	args = append(args, "--", "/bin/sh", "-c", switchScript)
	a := exec.Command("systemd-run", args...)
	a.SysProcAttr = &syscall.SysProcAttr{Setpgid: true, Pdeathsig: syscall.SIGTERM}
	if err := a.Start(); err != nil {
		return err
	}
	protected.Store(a.Process.Pid, true)
	cg, err := procmem.WaitForScope(o.Proc, a.Process.Pid, unit+".scope", 3*time.Second)
	if err == nil {
		var fd int
		fd, err = syscall.Open(cg.Path, syscall.O_RDONLY|syscall.O_DIRECTORY|syscall.O_CLOEXEC, 0)
		if err == nil {
			p.anchor, p.cgroup, p.cgfd = a, &cg, fd
			return nil
		}
	}
	syscall.Kill(-a.Process.Pid, syscall.SIGKILL)
	a.Wait()
	protected.Delete(a.Process.Pid)
	return err
}

// dropScope ends the anchor (so systemd collects the scope) and forgets the cgroup.
func (p *process) dropScope() {
	if p.cgfd >= 0 {
		syscall.Close(p.cgfd)
		p.cgfd = -1
	}
	if p.anchor != nil {
		syscall.Kill(-p.anchor.Process.Pid, syscall.SIGKILL) // the shell and its sleep, not the trap
		p.anchor.Wait()
		protected.Delete(p.anchor.Process.Pid)
		p.anchor = nil
	}
	if p.cgroup != nil && p.cgroup.Own {
		p.cgroup = nil
	}
}

// release lets the scope go, once its numbers are read.
func (p *process) release() {
	select {
	case <-p.released:
	default:
		close(p.released)
	}
	<-p.gone
	protected.Delete(p.pid)
}

func (p *process) anchorPID() int {
	if p.anchor != nil {
		return p.anchor.Process.Pid
	}
	return -1
}

// members is every live process of the run, never the anchor's: the cgroup's when the run has one
// of its own, otherwise its process group, plus orphans reparented to the supervisor while it
// supervises one run alone.
func (p *process) members() []int {
	var pids []int
	if p.cgroup != nil && p.cgroup.Own {
		pids, _ = p.cgroup.Procs()
	} else {
		parent := os.Getpid()
		if runsActive() > 1 {
			parent = -1 // several runs: an orphan outside its group cannot be told apart
		}
		pids = procmem.Members(p.proc, p.pid, parent, p.anchorPID())
	}
	out := pids[:0]
	for _, pid := range pids {
		if pid == p.anchorPID() || pid == os.Getpid() {
			continue
		}
		st, err := procmem.Stat(p.proc, pid)
		if err != nil || st.State == 'Z' || (p.anchor != nil && st.PGID == p.anchorPID()) {
			continue
		}
		out = append(out, pid)
	}
	return out
}

// cleanup stops whatever the run left running once its leader has exited: SIGTERM, then SIGKILL
// after the grace period. It returns what it stopped, by name.
func (p *process) cleanup(grace time.Duration) []string {
	left := p.members()
	if len(left) == 0 {
		reapChildren(p.proc)
		return nil
	}
	var names []string
	for _, pid := range left {
		if st, err := procmem.Stat(p.proc, pid); err == nil {
			names = append(names, fmt.Sprintf("%s (%d)", st.Comm, pid))
		}
		syscall.Kill(pid, syscall.SIGTERM)
	}
	deadline := time.Now().Add(grace)
	for time.Now().Before(deadline) && len(p.members()) > 0 {
		reapChildren(p.proc)
		time.Sleep(20 * time.Millisecond)
	}
	if rest := p.members(); len(rest) > 0 {
		for _, pid := range rest {
			syscall.Kill(pid, syscall.SIGKILL)
		}
		for end := time.Now().Add(time.Second); time.Now().Before(end) && len(p.members()) > 0; {
			reapChildren(p.proc)
			time.Sleep(10 * time.Millisecond)
		}
	}
	reapChildren(p.proc)
	return names
}

// terminate signals the run's leader alone. uv forwards SIGINT and SIGTERM to its child: signalling
// the whole group as well would deliver every signal twice.
func (p *process) terminate(sig syscall.Signal) {
	if p.cmd != nil && p.cmd.Process != nil {
		p.cmd.Process.Signal(sig)
	}
}

// kill ends every process of the run at once.
func (p *process) kill() {
	syscall.Kill(-p.pid, syscall.SIGKILL)
	for _, pid := range p.members() {
		syscall.Kill(pid, syscall.SIGKILL)
	}
}

// maxRSS is the largest resident size any process of the tree reached, from wait4's accounting
// (ru_maxrss, in KiB on Linux).
func (p *process) maxRSS() int64 {
	if p.cmd == nil || p.cmd.ProcessState == nil {
		return 0
	}
	if ru, ok := p.cmd.ProcessState.SysUsage().(*syscall.Rusage); ok {
		return ru.Maxrss * 1024
	}
	return 0
}

type outcome struct {
	status, signal, note string
	code                 int
}

// status turns how the leader ended, why the supervisor stopped it (if it did), and the cgroup's
// out-of-memory count into a status.
func (p *process) status(uv bool, why string, oomKills int64) outcome {
	st := p.cmd.ProcessState
	if st == nil {
		return outcome{status: "failed", code: 1, note: fmt.Sprint(p.waitErr)}
	}
	ws, _ := st.Sys().(syscall.WaitStatus)
	var o outcome
	switch {
	case ws.Signaled():
		o.code, o.signal = 128+int(ws.Signal()), signalName(ws.Signal())
	case uv && st.ExitCode() > 128 && st.ExitCode() <= 128+64:
		// uv reports a child killed by a signal the shell's way, as 128 + the signal.
		o.code, o.signal = st.ExitCode(), signalName(syscall.Signal(st.ExitCode()-128))
	default:
		o.code = st.ExitCode()
	}
	exact := p.cgroup != nil
	switch {
	case oomKills > 0:
		o.status = "oom"
		o.note = fmt.Sprintf("the kernel killed it for memory (its cgroup counted %d out-of-memory kills)", oomKills)
		if o.code == 0 {
			o.code = 137
		}
	case why == "cancelled", why == "timeout", why == "interrupted":
		o.status = why
	case o.signal == "SIGKILL" && exact:
		o.status, o.note = "killed", "killed by SIGKILL, not for memory: its cgroup counted no out-of-memory kill"
	case o.signal == "SIGKILL":
		o.status = "killed"
		o.note = "killed by SIGKILL, which is how the kernel's out-of-memory killer ends a process; with no cgroup this cannot be confirmed"
	case o.signal != "":
		o.status = "killed"
	case o.code == 0:
		o.status = "ok"
	default:
		o.status = "failed"
	}
	return o
}

func signalName(s syscall.Signal) string {
	names := map[syscall.Signal]string{syscall.SIGKILL: "SIGKILL", syscall.SIGTERM: "SIGTERM", syscall.SIGINT: "SIGINT", syscall.SIGSEGV: "SIGSEGV",
		syscall.SIGABRT: "SIGABRT", syscall.SIGBUS: "SIGBUS", syscall.SIGHUP: "SIGHUP", syscall.SIGPIPE: "SIGPIPE", syscall.SIGQUIT: "SIGQUIT"}
	if n, ok := names[s]; ok {
		return n
	}
	return fmt.Sprintf("signal %d", int(s))
}

// ------------------------------------------------------------------------- orphans and signals

var active sync.Map // run ID → true, while it runs

func runsActive() int {
	n := 0
	active.Range(func(_, _ any) bool { n++; return true })
	return n
}

// reapChildren reaps children of the supervisor that have exited and are nobody else's to wait
// for: orphans the run left, reparented here.
func reapChildren(proc string) {
	self := os.Getpid()
	entries, err := os.ReadDir(proc)
	if err != nil {
		return
	}
	for _, d := range entries {
		pid, err := strconv.Atoi(d.Name())
		if err != nil {
			continue
		}
		if _, ok := protected.Load(pid); ok {
			continue
		}
		if st, err := procmem.Stat(proc, pid); err == nil && st.PPID == self && st.State == 'Z' {
			var ws syscall.WaitStatus
			syscall.Wait4(pid, &ws, syscall.WNOHANG, nil)
		}
	}
}

func reapOrphans(proc string, p *process) (stop func()) {
	done := make(chan struct{})
	go func() {
		t := time.NewTicker(time.Second)
		defer t.Stop()
		for {
			select {
			case <-done:
				return
			case <-t.C:
				reapChildren(proc)
			}
		}
	}()
	return func() { close(done) }
}

// interrupter forwards signals to the run, enforces its time limit and honours cancellation.
type interrupter struct {
	mu    sync.Mutex
	why   string
	stopc chan struct{}
	done  chan struct{}
}

func (i *interrupter) set(why string) {
	i.mu.Lock()
	if i.why == "" {
		i.why = why
	}
	i.mu.Unlock()
}

func (i *interrupter) reason() string { i.mu.Lock(); defer i.mu.Unlock(); return i.why }

func (i *interrupter) stop() { close(i.stopc); <-i.done }

// handleSignals: the first SIGINT, SIGTERM or SIGHUP goes to the leader (SIGHUP as SIGTERM); a
// second ends everything at once. SIGUSR1 is `gongoozler kill`: a cancellation. A time limit or a
// cancellation sends SIGTERM, then SIGKILL if the run is still there five grace periods later.
func handleSignals(ctx context.Context, o Options, p *process) *interrupter {
	in := &interrupter{stopc: make(chan struct{}), done: make(chan struct{})}
	var sigs chan os.Signal
	if o.Signals {
		sigs = make(chan os.Signal, 4)
		signal.Notify(sigs, syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP, syscall.SIGUSR1)
	}
	var limit <-chan time.Time
	if o.Timeout > 0 {
		t := time.NewTimer(o.Timeout)
		defer func() {
			go func() { <-in.done; t.Stop() }()
		}()
		limit = t.C
	}
	go func() {
		defer close(in.done)
		if sigs != nil {
			defer signal.Stop(sigs)
		}
		n := 0
		var force <-chan time.Time
		hit := func(why string, sig syscall.Signal) {
			n++
			in.set(why)
			if n == 1 {
				p.terminate(sig)
				force = time.After(5 * o.Grace)
			} else {
				p.kill()
			}
		}
		cancelled := ctx.Done()
		for {
			select {
			case <-in.stopc:
				return
			case s := <-sigs:
				switch sig := s.(syscall.Signal); sig {
				case syscall.SIGUSR1:
					hit("cancelled", syscall.SIGTERM)
				case syscall.SIGHUP:
					hit("interrupted", syscall.SIGTERM)
				default:
					hit("interrupted", sig)
				}
			case <-limit:
				limit = nil
				hit("timeout", syscall.SIGTERM)
			case <-cancelled:
				cancelled = nil
				hit("cancelled", syscall.SIGTERM)
			case <-force:
				force = nil
				p.kill()
			}
		}
	}()
	return in
}
