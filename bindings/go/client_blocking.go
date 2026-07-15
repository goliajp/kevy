package kevy

import (
	"context"
	"strconv"
	"time"
)

// Blocking pops: BLPOP / BRPOP / BZPOPMIN (contract §3.14). Both backends
// block for real. timeout == nil waits forever (wire 0); a Some(zero) is
// rejected (ambiguous, since wire 0 already means forever). The timeout is
// sent as fractional seconds. Pass context.Background() to let the wire
// timeout govern; a ctx deadline is an extra client-side bound.

// KV is a (key, value) blocking-pop hit.
type KV struct {
	Key   []byte
	Value []byte
}

// ZPopHit is a (key, member, score) BZPOPMIN hit (contract §4.9).
type ZPopHit struct {
	Key    []byte
	Member []byte
	Score  float64
}

// BLPop blocks for a head element across keys (checked in order), popping
// and returning it. ok is false on timeout.
func (c *Client) BLPop(ctx context.Context, keys [][]byte, timeout *time.Duration) (KV, bool, error) {
	if err := checkBlockingArgs(keys, timeout); err != nil {
		return KV{}, false, err
	}
	if c.emb != nil {
		return c.embBlockKV(ctx, "LPOP", keys, timeout)
	}
	return c.popKV(ctx, "BLPOP", keys, timeout)
}

// BRPop is BLPop from the tail.
func (c *Client) BRPop(ctx context.Context, keys [][]byte, timeout *time.Duration) (KV, bool, error) {
	if err := checkBlockingArgs(keys, timeout); err != nil {
		return KV{}, false, err
	}
	if c.emb != nil {
		return c.embBlockKV(ctx, "RPOP", keys, timeout)
	}
	return c.popKV(ctx, "BRPOP", keys, timeout)
}

// BZPopMin blocks for the lowest-scored member across sorted sets. ok is
// false on timeout. (There is no BZPOPMAX server-side.)
func (c *Client) BZPopMin(ctx context.Context, keys [][]byte, timeout *time.Duration) (ZPopHit, bool, error) {
	if err := checkBlockingArgs(keys, timeout); err != nil {
		return ZPopHit{}, false, err
	}
	if c.emb != nil {
		return c.embBlockZ(ctx, keys, timeout)
	}
	r, err := c.exec(ctx, blockingArgv("BZPOPMIN", keys, timeout))
	if err != nil {
		return ZPopHit{}, false, err
	}
	switch {
	case r.Kind == KindArray && len(r.Array) == 3:
		k, m, s := r.Array[0], r.Array[1], r.Array[2]
		if k.Kind != KindBulk || m.Kind != KindBulk {
			return ZPopHit{}, false, unexpectedReply(k)
		}
		score, perr := scoreOf(s)
		if perr != nil {
			return ZPopHit{}, false, perr
		}
		return ZPopHit{Key: k.Bytes, Member: m.Bytes, Score: score}, true, nil
	case r.IsNil():
		return ZPopHit{}, false, nil
	case r.Kind == KindError:
		return ZPopHit{}, false, replyErr(r)
	default:
		return ZPopHit{}, false, unexpectedReply(r)
	}
}

func (c *Client) popKV(ctx context.Context, verb string, keys [][]byte, timeout *time.Duration) (KV, bool, error) {
	r, err := c.exec(ctx, blockingArgv(verb, keys, timeout))
	if err != nil {
		return KV{}, false, err
	}
	switch {
	case r.Kind == KindArray && len(r.Array) == 2:
		k, v := r.Array[0], r.Array[1]
		if k.Kind != KindBulk || v.Kind != KindBulk {
			return KV{}, false, unexpectedReply(k)
		}
		return KV{Key: k.Bytes, Value: v.Bytes}, true, nil
	case r.IsNil():
		return KV{}, false, nil
	case r.Kind == KindError:
		return KV{}, false, replyErr(r)
	default:
		return KV{}, false, unexpectedReply(r)
	}
}

func checkBlockingArgs(keys [][]byte, timeout *time.Duration) error {
	if len(keys) == 0 {
		return errInvalidInput("blocking pop needs at least one key")
	}
	if timeout != nil && *timeout == 0 {
		return errInvalidInput("timeout Some(0) is ambiguous (wire 0 = wait forever); use nil to wait forever")
	}
	return nil
}

func blockingArgv(verb string, keys [][]byte, timeout *time.Duration) [][]byte {
	argv := make([][]byte, 0, len(keys)+2)
	argv = append(argv, bs(verb))
	argv = append(argv, keys...)
	if timeout == nil {
		argv = append(argv, bs("0"))
	} else {
		argv = append(argv, []byte(strconv.FormatFloat(timeout.Seconds(), 'g', -1, 64)))
	}
	return argv
}

// embBlockKV emulates BLPOP/BRPOP on the embedded backend by polling the
// non-blocking pop until data arrives or the timeout elapses. The §5.1 C
// ABI exposes no blocking-pop symbol, so a pure-FFI port cannot park on
// the store condvar; this poll gives the same observable blocking.
func (c *Client) embBlockKV(ctx context.Context, verb string, keys [][]byte, timeout *time.Duration) (KV, bool, error) {
	deadline, hasDL := blockDeadline(timeout)
	for {
		for _, k := range keys {
			r, err := c.exec(ctx, [][]byte{bs(verb), k, bs("1")})
			if err != nil {
				return KV{}, false, err
			}
			if r.Kind == KindArray && len(r.Array) > 0 && r.Array[0].Kind == KindBulk {
				return KV{Key: k, Value: r.Array[0].Bytes}, true, nil
			}
			if r.Kind == KindBulk {
				return KV{Key: k, Value: r.Bytes}, true, nil
			}
		}
		if done, err := blockWait(ctx, deadline, hasDL); done {
			return KV{}, false, err
		}
	}
}

func (c *Client) embBlockZ(ctx context.Context, keys [][]byte, timeout *time.Duration) (ZPopHit, bool, error) {
	deadline, hasDL := blockDeadline(timeout)
	for {
		for _, k := range keys {
			r, err := c.exec(ctx, [][]byte{bs("ZPOPMIN"), k, bs("1")})
			if err != nil {
				return ZPopHit{}, false, err
			}
			if r.Kind == KindArray && len(r.Array) >= 2 && r.Array[0].Kind == KindBulk {
				score, perr := scoreOf(r.Array[1])
				if perr != nil {
					return ZPopHit{}, false, perr
				}
				return ZPopHit{Key: k, Member: r.Array[0].Bytes, Score: score}, true, nil
			}
		}
		if done, err := blockWait(ctx, deadline, hasDL); done {
			return ZPopHit{}, false, err
		}
	}
}

func blockDeadline(timeout *time.Duration) (time.Time, bool) {
	if timeout == nil {
		return time.Time{}, false
	}
	return time.Now().Add(*timeout), true
}

// blockWait sleeps one poll interval, returning done=true when the
// deadline elapses (miss) or ctx is cancelled (its error).
func blockWait(ctx context.Context, deadline time.Time, hasDL bool) (bool, error) {
	if err := ctx.Err(); err != nil {
		return true, ctxErr(ctx)
	}
	if hasDL && !time.Now().Before(deadline) {
		return true, nil
	}
	time.Sleep(5 * time.Millisecond)
	return false, nil
}

func scoreOf(r Reply) (float64, error) {
	switch r.Kind {
	case KindBulk, KindSimple:
		return parseFloat(r.Bytes)
	case KindDouble:
		return r.Double, nil
	default:
		return 0, unexpectedReply(r)
	}
}
