package kevy

import (
	"context"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

// Test harness: spawn a real kevy server for the remote-backend tests, and
// provide a "both backends" runner so the conformance checklist is
// exercised against embedded (mem://) and remote (kevy://) alike.

var bg = context.Background()

// freePort grabs an OS-assigned free TCP port on 127.0.0.1.
func freePort(t *testing.T) int {
	t.Helper()
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("freePort: %v", err)
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port
}

// serverBinary locates the kevy server binary, preferring the worktree
// build then the workspace build.
func serverBinary(t *testing.T) string {
	t.Helper()
	candidates := []string{
		"../../target/release/kevy",
		"../../../../../target/release/kevy",
		"/Users/doracawl/workspace/goliajp/kevy/target/release/kevy",
	}
	for _, c := range candidates {
		if abs, err := filepath.Abs(c); err == nil {
			if _, serr := os.Stat(abs); serr == nil {
				return abs
			}
		}
	}
	t.Skip("kevy server binary not found (build with: cargo build --release -p kevy)")
	return ""
}

// spawnedServer is a running kevy server for a test.
type spawnedServer struct {
	port int
	cmd  *exec.Cmd
}

// spawnServer starts a fresh single-node server on a free port + temp dir
// and waits until it answers PING. Extra args (e.g. --cluster --threads N)
// may be passed. Cleanup kills it on test end.
func spawnServer(t *testing.T, extra ...string) *spawnedServer {
	t.Helper()
	bin := serverBinary(t)
	port := freePort(t)
	dir := t.TempDir()
	args := append([]string{"--bind", "127.0.0.1", "--port", fmt.Sprint(port), "--dir", dir}, extra...)
	cmd := exec.Command(bin, args...)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		t.Fatalf("spawn server: %v", err)
	}
	s := &spawnedServer{port: port, cmd: cmd}
	t.Cleanup(func() {
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
	})
	s.waitReady(t)
	return s
}

func (s *spawnedServer) url() string {
	return fmt.Sprintf("kevy://127.0.0.1:%d", s.port)
}

func (s *spawnedServer) waitReady(t *testing.T) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		c, err := Connect(s.url())
		if err == nil {
			perr := c.Ping(bg)
			c.Close()
			if perr == nil {
				return
			}
		}
		time.Sleep(30 * time.Millisecond)
	}
	t.Fatalf("server on port %d never became ready", s.port)
}

// backend names a backend under test.
type backend struct {
	name string
	url  func(t *testing.T) (url string, cleanup func())
}

// bothBackends returns the embedded and remote backends for a test. The
// embedded URL is a unique named bus per subtest so pub/sub works and
// stores don't leak across tests; the remote URL points at a fresh server.
func bothBackends(t *testing.T) []backend {
	return []backend{
		{
			name: "embedded",
			url: func(t *testing.T) (string, func()) {
				u := fmt.Sprintf("mem://test-%d", uniqueID())
				return u, func() {}
			},
		},
		{
			name: "remote",
			url: func(t *testing.T) (string, func()) {
				s := spawnServer(t)
				return s.url(), func() {}
			},
		},
	}
}

var idCounter struct {
	sync.Mutex
	n int
}

func uniqueID() int {
	idCounter.Lock()
	defer idCounter.Unlock()
	idCounter.n++
	return idCounter.n
}

// mustConnect opens a client or fails the test.
func mustConnect(t *testing.T, url string) *Client {
	t.Helper()
	c, err := Connect(url)
	if err != nil {
		t.Fatalf("Connect(%q): %v", url, err)
	}
	t.Cleanup(c.Close)
	return c
}

func b(s string) []byte { return []byte(s) }
