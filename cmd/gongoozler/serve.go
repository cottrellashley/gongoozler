package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/cottrellashley/gongoozler/internal/client"
	"github.com/cottrellashley/gongoozler/internal/header"
	"github.com/cottrellashley/gongoozler/internal/server"
	"github.com/cottrellashley/gongoozler/internal/version"
	"github.com/cottrellashley/gongoozler/internal/worker"
)

func serverCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("server", "[flags]", stderr)
	listen := fs.String("listen", "127.0.0.1:8150", "the address to serve on")
	data := fs.String("data", "", "where the server keeps runs and blobs (default STATE/server)")
	ttl := fs.Duration("ttl", 15*time.Second, "how long a lease lasts past a worker's last heartbeat")
	attempts := fs.Int("attempts", 3, "how many times a job runs whose worker goes quiet")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *data == "" {
		state, ok := stateDir(stderr)
		if !ok {
			return 1
		}
		*data = filepath.Join(state, "server")
	}
	token := os.Getenv("GONGOOZLER_TOKEN")
	s, err := server.New(server.Config{Dir: *data, Token: token, TTL: *ttl, MaxAttempts: *attempts, Version: version.Version})
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	defer s.Close()
	ln, err := net.Listen("tcp", *listen)
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	host, _, _ := net.SplitHostPort(*listen)
	if token == "" && host != "127.0.0.1" && host != "localhost" && host != "::1" {
		fmt.Fprintln(stderr, "gongoozler: serving beyond this machine with no token: set GONGOOZLER_TOKEN")
	}
	fmt.Fprintf(stdout, "gongoozler server %s: http://%s (data %s)\n", version.Version, ln.Addr(), *data)
	hs := &http.Server{Handler: s, ReadHeaderTimeout: 10 * time.Second}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	errc := make(chan error, 1)
	go func() { errc <- hs.Serve(ln) }()
	select {
	case err := <-errc:
		return failf(stderr, 1, "%v", err)
	case <-ctx.Done():
	}
	s.Close() // ends the event streams and lease waits, so shutting down need not wait for them
	shut, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := hs.Shutdown(shut); err != nil && !errors.Is(err, context.DeadlineExceeded) {
		return failf(stderr, 1, "%v", err)
	}
	return 0
}

func workerCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("worker", "[flags]", stderr)
	serverURL := fs.String("server", os.Getenv("GONGOOZLER_SERVER"), "the server's URL (default $GONGOOZLER_SERVER)")
	name := fs.String("name", "", "the worker's name (default HOST-PID)")
	slots := fs.Int("slots", 1, "runs at once")
	memory := fs.String("memory", "", "the memory it may give its runs, such as 64GiB (default: what is available)")
	cpus := fs.Float64("cpus", 0, "the CPUs it may give its runs (default: all)")
	work := fs.String("work", "", "where jobs' files go while they run (default STATE/worker)")
	docker := fs.Bool("docker", false, "run each job in a container of its own")
	image := fs.String("image", "gongoozler", "with --docker: the image to run jobs in")
	network := fs.String("network", "", "with --docker: the containers' network")
	cgroup := fs.String("cgroup", "auto", "without --docker: auto, off or require (as for run)")
	launcher := fs.String("launcher", "", "without --docker: the command to give files to, instead of `uv run --script` (for tests)")
	heartbeat := fs.Duration("heartbeat", 0, "how often leases are renewed (default: a third of the server's TTL)")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *serverURL == "" {
		return failf(stderr, 2, "which server? --server URL or $GONGOOZLER_SERVER")
	}
	var mem int64
	if *memory != "" {
		var err error
		if mem, err = header.Bytes(*memory); err != nil {
			return failf(stderr, 2, "--memory: %v", err)
		}
	}
	if *work == "" {
		state, ok := stateDir(stderr)
		if !ok {
			return 1
		}
		*work = filepath.Join(state, "worker")
	}
	var l worker.Launcher
	if *docker {
		l = &worker.Docker{Image: *image, Network: *network}
	} else {
		pl := worker.Process{Cgroup: *cgroup, Version: version.Version}
		if *launcher != "" {
			pl.Launcher = strings.Fields(*launcher)
		}
		l = pl
	}
	cfg := worker.Config{
		Client: client.New(*serverURL, os.Getenv("GONGOOZLER_TOKEN")), Name: *name, Slots: *slots, Memory: mem, CPUs: *cpus,
		Work: *work, Launcher: l, Heartbeat: *heartbeat, Log: stderr,
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	fmt.Fprintf(stdout, "gongoozler worker %s: taking jobs from %s, %s at a time%s\n", version.Version, *serverURL, strconv.Itoa(*slots), map[bool]string{true: ", in containers of " + *image}[*docker])
	if err := worker.Run(ctx, cfg); err != nil {
		return failf(stderr, 1, "%v", err)
	}
	return 0
}
