// Command gongoozler runs backtest files and watches them run (spec/design.md).
//
//	gongoozler run [flags] FILE [ARGS...]     run a file, here or on a server, and watch it
//	gongoozler sweep [flags] FILE [ARGS...]   run a file over a grid of parameters, and rank the runs
//	gongoozler ls [--json] [--remote]         list runs, here or on the server
//	gongoozler events [--follow] RUN          print a run's events, as JSON lines
//	gongoozler attach [--plain] RUN           watch a run as it goes
//	gongoozler kill [--force] RUN             stop a run
//	gongoozler diff RUN RUN                   compare two runs' inputs and results
//	gongoozler rerun RUN                      run a run again, and check its results reproduce
//	gongoozler pull RUN                       fetch a remote run's record, events and artifacts
//	gongoozler server [flags]                 keep runs and queue them for workers
//	gongoozler worker [flags]                 run a server's jobs, here or in containers
//	gongoozler version
//
// A run is shown live on a terminal, as plain lines otherwise (for agents and logs), or as JSON
// events with --json. `run` ends with the run's own exit code.
package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"strings"

	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/runs"
	"github.com/cottrellashley/gongoozler/internal/sweep"
	"github.com/cottrellashley/gongoozler/internal/term"
	"github.com/cottrellashley/gongoozler/internal/version"
)

func main() {
	os.Exit(cli(os.Args[1:], os.Stdout, os.Stderr))
}

const usageText = `gongoozler runs backtest files and watches them run.

  gongoozler run [flags] FILE [ARGS...]     run a file, here or on a server, and watch it
  gongoozler sweep [flags] FILE [ARGS...]   run a file over a grid of parameters, and rank the runs
  gongoozler ls [--json] [--remote]         list runs, here or on the server
  gongoozler events [--follow] RUN          print a run's events, as JSON lines
  gongoozler attach [--plain] RUN           watch a run as it goes
  gongoozler kill [--force] RUN             stop a run
  gongoozler diff RUN RUN                   compare two runs' inputs and results
  gongoozler rerun RUN                      run a run again, and check its results reproduce
  gongoozler pull RUN                       fetch a remote run's record, events and artifacts
  gongoozler server [flags]                 keep runs and queue them for workers
  gongoozler worker [flags]                 run a server's jobs, here or in containers
  gongoozler version

A RUN is a run's ID or a unique prefix of one. 'gongoozler COMMAND -h' lists a command's flags.

Runs are kept under $GONGOOZLER_STATE (default ~/.local/state/gongoozler). A server is named by
$GONGOOZLER_SERVER, with the token $GONGOOZLER_TOKEN; $GONGOOZLER_PLACEMENT (auto, local or
remote) says where runs go by default.
`

func cli(args []string, stdout, stderr io.Writer) int {
	if len(args) == 0 {
		fmt.Fprint(stderr, usageText)
		return 2
	}
	cmds := map[string]func([]string, io.Writer, io.Writer) int{
		"run": runCmd, "sweep": sweepCmd, "ls": lsCmd, "events": eventsCmd, "attach": attachCmd, "kill": killCmd,
		"diff": diffCmd, "rerun": rerunCmd, "pull": pullCmd, "server": serverCmd, "worker": workerCmd,
	}
	switch args[0] {
	case "version", "--version":
		fmt.Fprintf(stdout, "gongoozler %s (protocol %d)\n", version.Version, events.Protocol)
		return 0
	case "help", "-h", "--help":
		fmt.Fprint(stdout, usageText)
		return 0
	}
	if fn, ok := cmds[args[0]]; ok {
		return fn(args[1:], stdout, stderr)
	}
	fmt.Fprintf(stderr, "gongoozler: no command %q\n\n%s", args[0], usageText)
	return 2
}

// flags is a command's flag set, printing to stderr, with its usage line.
func flags(name, usage string, stderr io.Writer) *flag.FlagSet {
	fs := flag.NewFlagSet(name, flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.Usage = func() {
		fmt.Fprintf(stderr, "usage: gongoozler %s %s\n", name, usage)
		fs.PrintDefaults()
	}
	return fs
}

// failf reports an error and returns the exit code for it.
func failf(stderr io.Writer, code int, format string, args ...any) int {
	fmt.Fprintf(stderr, "gongoozler: "+format+"\n", args...)
	return code
}

func stateDir(stderr io.Writer) (string, bool) {
	state, err := runs.StateDir()
	if err != nil {
		failf(stderr, 1, "%v", err)
		return "", false
	}
	return state, true
}

// paramFlag collects --param NAME=VALUE flags: each value as JSON when it is JSON, else a string.
type paramFlag map[string]any

func (p paramFlag) String() string { return "" }

func (p paramFlag) Set(s string) error {
	name, value, ok := strings.Cut(s, "=")
	if !ok || name == "" {
		return fmt.Errorf("%q is not NAME=VALUE", s)
	}
	p[name] = sweep.Value(value)
	return nil
}

// sweepFlag collects --param NAME=V1,V2,... flags: each a list of values to sweep.
type sweepFlag map[string][]any

func (p sweepFlag) String() string { return "" }

func (p sweepFlag) Set(s string) error {
	name, value, ok := strings.Cut(s, "=")
	if !ok || name == "" {
		return fmt.Errorf("%q is not NAME=VALUES", s)
	}
	p[name] = sweep.Values(value)
	return nil
}

// mode is how a run is shown: JSON when asked, plain when asked or when stderr is no terminal.
func mode(asJSON, plain bool) term.Mode {
	switch {
	case asJSON:
		return term.JSON
	case plain || !term.IsTerminal(os.Stderr.Fd()):
		return term.Plain
	}
	return term.Live
}

func renderer(m term.Mode, stdout, stderr io.Writer) *term.Renderer {
	return term.New(m, stdout, stderr, func() int { return term.Width(os.Stderr.Fd()) })
}

// show passes a stored event to a renderer, as the supervisor would have: in JSON mode as it is,
// otherwise output as lines.
func show(r *term.Renderer, m term.Mode, e events.Event) {
	if m != term.JSON && e.Kind() == "output" {
		r.Output(e.Str("stream"), e.Str("line"))
		return
	}
	r.Event(e)
}

// resolve finds a run by its ID or a unique prefix of one.
func resolve(state, prefix string) (runs.Meta, error) {
	all, err := runs.List(state)
	if err != nil {
		return runs.Meta{}, err
	}
	var hits []runs.Meta
	for _, m := range all {
		if m.ID == prefix {
			return m, nil
		}
		if strings.HasPrefix(m.ID, prefix) {
			hits = append(hits, m)
		}
	}
	switch len(hits) {
	case 1:
		return hits[0], nil
	case 0:
		return runs.Meta{}, fmt.Errorf("no run %q (gongoozler ls lists them)", prefix)
	}
	ids := make([]string, len(hits))
	for i, m := range hits {
		ids[i] = m.ID
	}
	return runs.Meta{}, errors.New("several runs start " + prefix + ": " + strings.Join(ids, ", "))
}

func printJSON(w io.Writer, v any) {
	enc := json.NewEncoder(w)
	enc.SetIndent("", "  ")
	enc.Encode(v)
}
