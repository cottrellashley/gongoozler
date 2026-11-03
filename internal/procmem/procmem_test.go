package procmem

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func TestAGroupsMemoryIncludesItsChildren(t *testing.T) {
	// A shell in its own group starts Python, which holds 200 MB. The trailing `true` keeps the
	// shell from replacing itself with Python, so the group has two processes.
	cmd := exec.Command("sh", "-c", `python3 -c "x = bytearray(200_000_000); import time; time.sleep(3)"; true`)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer cmd.Process.Kill()
	ctx, cancel := context.WithCancel(context.Background())
	w := Watch(ctx, "/proc", cmd.Process.Pid, 50*time.Millisecond, nil)
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) && w.Last().Peak < 190_000_000 {
		time.Sleep(50 * time.Millisecond)
	}
	cancel()
	w.Wait()
	s := w.Last()
	if s.Peak < 190_000_000 {
		t.Fatalf("peak %d bytes, want at least 190 MB", s.Peak)
	}
	if s.Procs < 2 {
		t.Fatalf("%d processes in the group, want the shell and Python", s.Procs)
	}
	if s.Available <= 0 {
		t.Fatalf("available %d", s.Available)
	}
}

func TestACommandNameWithParenthesesStillParses(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, "42"), 0o755); err != nil {
		t.Fatal(err)
	}
	stat := "42 (weird) name (x)) S 1 777 777 0 -1 4194560 0 0 0 0\n"
	if err := os.WriteFile(filepath.Join(dir, "42", "stat"), []byte(stat), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "42", "statm"), []byte("1000 250 10 1 0 100 0\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "meminfo"), []byte("MemTotal: 100 kB\nMemAvailable:    2048 kB\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	rss, procs, err := GroupRSS(dir, 777)
	if err != nil || procs != 1 || rss != 250*pageSize {
		t.Fatalf("rss %d procs %d err %v", rss, procs, err)
	}
	if a, err := Available(dir); err != nil || a != 2048*1024 {
		t.Fatalf("available %d err %v", a, err)
	}
}
