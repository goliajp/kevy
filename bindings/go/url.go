package kevy

import (
	"strconv"
	"strings"
	"sync"
)

// targetKind is the backend an input URL resolves to (contract §1.1).
type targetKind int

const (
	targetMemAnon  targetKind = iota // mem:// — fresh, never shared
	targetMemNamed                   // mem://<name> — shared by name
	targetFile                       // file://<path> — shared + persistent
	targetRemote                     // kevy:// / redis:// / tcp://
)

// target is a parsed connect URL.
type target struct {
	kind targetKind
	name string  // mem://<name>
	path string  // file://<path>
	host string  // remote host
	port uint16  // remote port (default 6379)
	db   *uint32 // remote /db index (nil = none); always nil for tcp://
	url  string  // original URL
}

// registryKey returns the process-global map key for shared embedded
// targets (mem://<name>, file://<path>), or "" for the unshared ones.
func (t target) registryKey() string {
	switch t.kind {
	case targetMemNamed:
		return "mem://" + t.name
	case targetFile:
		return "file://" + t.path
	default:
		return ""
	}
}

// parseConnectURL resolves a connect URL to a target, rejecting TLS/AUTH
// and unknown schemes before any I/O (contract §1.1).
func parseConnectURL(url string) (target, error) {
	scheme, rest, ok := strings.Cut(url, "://")
	if !ok {
		return target{}, errInvalidInput("URL missing '://'")
	}
	switch scheme {
	case "mem":
		if rest == "" {
			return target{kind: targetMemAnon, url: url}, nil
		}
		return target{kind: targetMemNamed, name: rest, url: url}, nil
	case "file":
		if rest == "" {
			return target{}, errInvalidInput("file:// URL must include a path (e.g. file:///var/lib/myapp)")
		}
		return target{kind: targetFile, path: rest, url: url}, nil
	case "kevy", "redis", "tcp":
		return parseRemote(scheme, rest, url)
	case "rediss", "kevys":
		return target{}, errUnsupported("TLS schemes (rediss://, kevys://) are unsupported — kevy has no TLS")
	default:
		return target{}, errInvalidInput("unknown URL scheme '" + scheme + "://'")
	}
}

func parseRemote(scheme, rest, url string) (target, error) {
	if strings.Contains(rest, "@") {
		return target{}, errUnsupported("userinfo (user:pass@host) is unsupported — kevy has no AUTH")
	}
	authority := rest
	var path string
	if i := strings.IndexByte(rest, '/'); i >= 0 {
		authority = rest[:i]
		path = rest[i+1:]
	}
	host, port, err := parseAuthority(authority)
	if err != nil {
		return target{}, err
	}
	t := target{kind: targetRemote, host: host, port: port, url: url}
	// tcp:// is the raw form: it never carries a db (contract §1.1 — it
	// ignores any /db and does no SELECT). kevy:// / redis:// honour /N.
	if scheme != "tcp" && path != "" {
		n, perr := strconv.ParseUint(path, 10, 32)
		if perr != nil {
			return target{}, errInvalidInput("bad db index: '" + path + "' (expected a non-negative integer)")
		}
		db := uint32(n)
		t.db = &db
	}
	return t, nil
}

func parseAuthority(authority string) (string, uint16, error) {
	host := authority
	port := uint16(6379)
	if i := strings.LastIndexByte(authority, ':'); i >= 0 {
		host = authority[:i]
		p, err := strconv.ParseUint(authority[i+1:], 10, 16)
		if err != nil {
			return "", 0, errInvalidInput("bad port: " + authority[i+1:])
		}
		port = uint16(p)
	}
	if host == "" {
		return "", 0, errInvalidInput("empty host")
	}
	return host, port, nil
}

// --- process-global embedded registry (contract §1.3) -------------------
//
// A URL-keyed weak map: two Connect calls with the same mem://<name> or
// file://<path> share one Store (and one pub/sub bus). Entries evict when
// the last strong handle drops — modelled here with reference counting.

type sharedStore struct {
	db   *DB
	refs int
}

var registry = struct {
	mu sync.Mutex
	m  map[string]*sharedStore
}{m: make(map[string]*sharedStore)}

// resolveStore opens (or shares) the embedded store for an embedded
// target and returns it plus the registry key to release later.
func resolveStore(t target) (*DB, string, error) {
	key := t.registryKey()
	if key != "" {
		registry.mu.Lock()
		if s, ok := registry.m[key]; ok {
			s.refs++
			registry.mu.Unlock()
			return s.db, key, nil
		}
		registry.mu.Unlock()
	}
	db, err := openEmbedded(t)
	if err != nil {
		return nil, "", err
	}
	if key == "" {
		return db, "", nil
	}
	registry.mu.Lock()
	defer registry.mu.Unlock()
	// Lost a race: another goroutine registered first — use theirs.
	if s, ok := registry.m[key]; ok {
		s.refs++
		db.Close()
		return s.db, key, nil
	}
	registry.m[key] = &sharedStore{db: db, refs: 1}
	return db, key, nil
}

func openEmbedded(t target) (*DB, error) {
	switch t.kind {
	case targetMemAnon, targetMemNamed:
		return OpenMem()
	case targetFile:
		return Open(t.path)
	default:
		return nil, errInvalidInput("resolveStore called on a non-embedded target")
	}
}

// releaseStore drops one reference to a shared store, closing it when the
// last handle goes. Anonymous stores (key == "") are closed directly.
func releaseStore(key string, db *DB) {
	if key == "" {
		db.Close()
		return
	}
	registry.mu.Lock()
	defer registry.mu.Unlock()
	s, ok := registry.m[key]
	if !ok {
		return
	}
	s.refs--
	if s.refs <= 0 {
		s.db.Close()
		delete(registry.m, key)
	}
}
