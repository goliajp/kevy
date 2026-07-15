package kevy

import (
	"context"
	"strconv"
	"time"
)

// Client is one open connection to a kevy backend — opaque about whether
// that backend is the in-process embedded engine (mem:// / file://) or a
// remote RESP server (kevy:// / redis:// / tcp://). A single Connect(url)
// chooses; the same business code runs against either by changing only
// the URL (contract §1.1).
//
// Every command is exposed on both a blocking face (the methods here,
// which take a context.Context for cancellation/timeout) and an async
// face (Client.Async, contract §1.4). Both agree because the async face
// delegates to the blocking methods.
//
// Methods return (T, error); the error is a *KevyError inspectable via
// errors.As / IsKind / StoreErrorOf (contract §2).
type Client struct {
	remote *respConn
	emb    *DB
	embKey string // registry key, for release on Close
	url    string
}

// Connect opens a backend chosen by URL scheme (contract §1.1). Rejects
// TLS/AUTH and unknown schemes before any I/O. For kevy:// / redis://
// with a /db it issues one SELECT round-trip.
func Connect(url string) (*Client, error) {
	t, err := parseConnectURL(url)
	if err != nil {
		return nil, err
	}
	if t.kind == targetRemote {
		conn, derr := dialResp(t.host, t.port)
		if derr != nil {
			return nil, derr
		}
		if t.db != nil {
			if serr := conn.selectDB(*t.db); serr != nil {
				conn.close()
				return nil, serr
			}
		}
		return &Client{remote: conn, url: url}, nil
	}
	db, key, oerr := resolveStore(t)
	if oerr != nil {
		return nil, oerr
	}
	return &Client{emb: db, embKey: key, url: url}, nil
}

// Close releases the connection or drops one reference to the shared
// embedded store. Safe to call once.
func (c *Client) Close() {
	if c.remote != nil {
		c.remote.close()
		c.remote = nil
	}
	if c.emb != nil {
		releaseStore(c.embKey, c.emb)
		c.emb = nil
	}
}

// IsEmbedded reports whether this client is backed by the in-process
// engine (vs a remote RESP server).
func (c *Client) IsEmbedded() bool { return c.emb != nil }

// Async returns the async face over this same client (contract §1.4).
func (c *Client) Async() *Async { return &Async{c: c} }

// exec runs one argv against whichever backend backs the client and hands
// back the raw Reply. The returned error is only for transport / ABI
// failure; a protocol -ERR arrives as a Reply with Kind == KindError.
func (c *Client) exec(ctx context.Context, argv [][]byte) (Reply, error) {
	if c.emb != nil {
		r, err := c.emb.CmdBytes(argv...)
		if err != nil {
			return Reply{}, errClosed()
		}
		return r, nil
	}
	if c.remote != nil {
		return c.remote.request(ctx, argv)
	}
	return Reply{}, errClosed()
}

// requireRemote gates a remote-only feature (contract §3.8/§3.12/§3.13):
// the embedded backend answers Unsupported, pointing at the escape hatch.
func (c *Client) requireRemote(feature string) (*respConn, error) {
	if c.remote != nil {
		return c.remote, nil
	}
	return nil, errUnsupported(feature + " is remote-only; on the embedded backend use the DB.Cmd raw path")
}

// replyErr converts a Reply::Error into a *KevyError, classifying known
// store-semantic errors into KindStore (contract §6: a wrong-type op
// surfaces Store(WrongType), INCR non-numeric → Store(NotInteger)) and
// preserving any other wire text verbatim as a Protocol error (§2.2).
func replyErr(r Reply) *KevyError {
	if k, ok := classifyStoreError(r.Bytes); ok {
		return errStore(k)
	}
	return &KevyError{Kind: KindProtocol, Msg: r.Str()}
}

// Do is the mandatory raw-argv escape hatch (contract §7): run any verb
// and get the raw Reply back, so no server capability is unreachable. A
// -ERR reply is returned as a Reply with Kind == KindError (data, not a
// Go error), matching the pipeline inline-error convention.
func (c *Client) Do(ctx context.Context, argv ...[]byte) (Reply, error) {
	if len(argv) == 0 {
		return Reply{}, errInvalidInput("Do needs at least a verb")
	}
	return c.exec(ctx, argv)
}

// --- small typed-reply adapters -----------------------------------------

func (c *Client) execInt(ctx context.Context, argv [][]byte) (int64, error) {
	r, err := c.exec(ctx, argv)
	if err != nil {
		return 0, err
	}
	switch r.Kind {
	case KindInt:
		return r.Int, nil
	case KindError:
		return 0, replyErr(r)
	default:
		return 0, unexpectedReply(r)
	}
}

func (c *Client) execCount(ctx context.Context, argv [][]byte) (int64, error) {
	n, err := c.execInt(ctx, argv)
	if err != nil {
		return 0, err
	}
	if n < 0 {
		return 0, errProtocol("expected non-negative count, got %d", n)
	}
	return n, nil
}

func (c *Client) execOK(ctx context.Context, argv [][]byte) error {
	r, err := c.exec(ctx, argv)
	if err != nil {
		return err
	}
	switch {
	case r.Kind == KindSimple && r.Str() == "OK":
		return nil
	case r.Kind == KindError:
		return replyErr(r)
	default:
		return unexpectedReply(r)
	}
}

func (c *Client) execBulks(ctx context.Context, argv [][]byte) ([][]byte, error) {
	r, err := c.exec(ctx, argv)
	if err != nil {
		return nil, err
	}
	switch r.Kind {
	case KindArray, KindSet:
		return arrayToBulks(r.Array)
	case KindError:
		return nil, replyErr(r)
	default:
		return nil, unexpectedReply(r)
	}
}

func arrayToBulks(items []Reply) ([][]byte, error) {
	out := make([][]byte, 0, len(items))
	for _, it := range items {
		switch it.Kind {
		case KindBulk, KindSimple:
			out = append(out, it.Bytes)
		case KindNil, KindNull:
			out = append(out, nil)
		default:
			return nil, unexpectedReply(it)
		}
	}
	return out, nil
}

func durationToMs(ttl time.Duration) int64 {
	ms := ttl.Milliseconds()
	if ms < 0 {
		return 0
	}
	return ms
}

// --- core string / generic key (contract §3.1) --------------------------

// Ping sends PING (embedded always OK).
func (c *Client) Ping(ctx context.Context) error {
	if c.emb != nil {
		return nil
	}
	r, err := c.exec(ctx, argv1("PING"))
	if err != nil {
		return err
	}
	switch {
	case r.Kind == KindSimple && r.Str() == "PONG":
		return nil
	case r.Kind == KindError:
		return replyErr(r)
	default:
		return unexpectedReply(r)
	}
}

// Set stores value under key unconditionally (contract §3.1).
func (c *Client) Set(ctx context.Context, key, value []byte) error {
	return c.execOK(ctx, [][]byte{bs("SET"), key, value})
}

// Get fetches key; ok is false when absent or expired.
func (c *Client) Get(ctx context.Context, key []byte) (value []byte, ok bool, err error) {
	r, err := c.exec(ctx, [][]byte{bs("GET"), key})
	if err != nil {
		return nil, false, err
	}
	switch r.Kind {
	case KindBulk:
		return r.Bytes, true, nil
	case KindNil, KindNull:
		return nil, false, nil
	case KindError:
		return nil, false, replyErr(r)
	default:
		return nil, false, unexpectedReply(r)
	}
}

// Del removes keys, returning how many were actually removed.
func (c *Client) Del(ctx context.Context, keys ...[]byte) (int64, error) {
	return c.execCount(ctx, prepend("DEL", keys))
}

// Exists counts keys present (a repeated key counts each time).
func (c *Client) Exists(ctx context.Context, keys ...[]byte) (int64, error) {
	return c.execCount(ctx, prepend("EXISTS", keys))
}

// Incr adds 1 to key and returns the post-increment value.
func (c *Client) Incr(ctx context.Context, key []byte) (int64, error) {
	return c.execInt(ctx, [][]byte{bs("INCR"), key})
}

// IncrBy adds delta (negative = DECRBY) and returns the post value.
func (c *Client) IncrBy(ctx context.Context, key []byte, delta int64) (int64, error) {
	return c.execInt(ctx, [][]byte{bs("INCRBY"), key, itob(delta)})
}

// Expire sets key's TTL (wire PEXPIRE); false when the key does not exist.
func (c *Client) Expire(ctx context.Context, key []byte, ttl time.Duration) (bool, error) {
	return c.execBool(ctx, [][]byte{bs("PEXPIRE"), key, itob(durationToMs(ttl))})
}

// Persist removes key's TTL; false when there was none.
func (c *Client) Persist(ctx context.Context, key []byte) (bool, error) {
	return c.execBool(ctx, [][]byte{bs("PERSIST"), key})
}

// TTLMs returns ms remaining (wire PTTL): -2 no key, -1 no TTL.
func (c *Client) TTLMs(ctx context.Context, key []byte) (int64, error) {
	return c.execInt(ctx, [][]byte{bs("PTTL"), key})
}

// TypeOf returns the value's type name, or "none".
func (c *Client) TypeOf(ctx context.Context, key []byte) (string, error) {
	r, err := c.exec(ctx, [][]byte{bs("TYPE"), key})
	if err != nil {
		return "", err
	}
	switch r.Kind {
	case KindSimple:
		return r.Str(), nil
	case KindError:
		return "", replyErr(r)
	default:
		return "", unexpectedReply(r)
	}
}

// DBSize returns the live key count.
func (c *Client) DBSize(ctx context.Context) (int64, error) {
	return c.execCount(ctx, argv1("DBSIZE"))
}

// FlushAll wipes the store.
func (c *Client) FlushAll(ctx context.Context) error {
	return c.execOK(ctx, argv1("FLUSHALL"))
}

// SetWithTTL is an atomic SET … PX ttl_ms.
func (c *Client) SetWithTTL(ctx context.Context, key, value []byte, ttl time.Duration) error {
	return c.execOK(ctx, [][]byte{bs("SET"), key, value, bs("PX"), itob(durationToMs(ttl))})
}

// MGet fetches many keys; a missing/wrong-type key yields nil, in order.
func (c *Client) MGet(ctx context.Context, keys ...[]byte) ([][]byte, error) {
	r, err := c.exec(ctx, prepend("MGET", keys))
	if err != nil {
		return nil, err
	}
	switch r.Kind {
	case KindArray:
		out := make([][]byte, 0, len(r.Array))
		for _, it := range r.Array {
			switch it.Kind {
			case KindBulk:
				out = append(out, it.Bytes)
			case KindNil, KindNull:
				out = append(out, nil)
			default:
				return nil, unexpectedReply(it)
			}
		}
		return out, nil
	case KindError:
		return nil, replyErr(r)
	default:
		return nil, unexpectedReply(r)
	}
}

// MSet sets every pair atomically. pairs must be even-length [k,v,k,v,…].
func (c *Client) MSet(ctx context.Context, pairs ...[]byte) error {
	if len(pairs)%2 != 0 {
		return errInvalidInput("MSet needs an even number of key/value arguments")
	}
	argv := make([][]byte, 0, len(pairs)+1)
	argv = append(argv, bs("MSET"))
	argv = append(argv, pairs...)
	return c.execOK(ctx, argv)
}

// Publish delivers message to channel, returning subscribers reached.
func (c *Client) Publish(ctx context.Context, channel, message []byte) (int64, error) {
	return c.execCount(ctx, [][]byte{bs("PUBLISH"), channel, message})
}

func (c *Client) execBool(ctx context.Context, argv [][]byte) (bool, error) {
	r, err := c.exec(ctx, argv)
	if err != nil {
		return false, err
	}
	switch r.Kind {
	case KindInt:
		return r.Int == 1, nil
	case KindError:
		return false, replyErr(r)
	default:
		return false, unexpectedReply(r)
	}
}

// --- argv helpers -------------------------------------------------------

func bs(s string) []byte { return []byte(s) }

func itob(n int64) []byte { return []byte(strconv.FormatInt(n, 10)) }

func argv1(verb string) [][]byte { return [][]byte{bs(verb)} }

func prepend(verb string, rest [][]byte) [][]byte {
	out := make([][]byte, 0, len(rest)+1)
	out = append(out, bs(verb))
	out = append(out, rest...)
	return out
}
