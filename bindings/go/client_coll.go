package kevy

import (
	"context"
	"strconv"
)

// Collection-typed commands: hash, list, set, sorted set (contract
// §3.2–§3.5). Each runs the same argv against either backend; the
// embedded engine dispatches the full verb surface through the FFI cmd
// path, so set combines (SINTER/SUNION/SDIFF) run server-side there too.

// --- Hash (§3.2) --------------------------------------------------------

// HSet sets field/value pairs, returning newly-added (non-overwrite)
// fields. pairs must be [field,value,field,value,…].
func (c *Client) HSet(ctx context.Context, key []byte, pairs ...[]byte) (int64, error) {
	if len(pairs)%2 != 0 {
		return 0, errInvalidInput("HSet needs an even number of field/value arguments")
	}
	argv := make([][]byte, 0, len(pairs)+2)
	argv = append(argv, bs("HSET"), key)
	argv = append(argv, pairs...)
	return c.execCount(ctx, argv)
}

// HGet fetches one field; ok is false when key or field is absent.
func (c *Client) HGet(ctx context.Context, key, field []byte) (value []byte, ok bool, err error) {
	return c.execOptBulk(ctx, [][]byte{bs("HGET"), key, field})
}

// HDel removes fields, returning how many were removed.
func (c *Client) HDel(ctx context.Context, key []byte, fields ...[]byte) (int64, error) {
	return c.execCount(ctx, prepend2("HDEL", key, fields))
}

// HLen returns the field count (0 if absent).
func (c *Client) HLen(ctx context.Context, key []byte) (int64, error) {
	return c.execCount(ctx, [][]byte{bs("HLEN"), key})
}

// HGetAll returns a flat [f0,v0,f1,v1,…] (empty if absent).
func (c *Client) HGetAll(ctx context.Context, key []byte) ([][]byte, error) {
	return c.execBulks(ctx, [][]byte{bs("HGETALL"), key})
}

// HKeys returns the field names (empty if absent).
func (c *Client) HKeys(ctx context.Context, key []byte) ([][]byte, error) {
	return c.execBulks(ctx, [][]byte{bs("HKEYS"), key})
}

// HVals returns the field values (empty if absent).
func (c *Client) HVals(ctx context.Context, key []byte) ([][]byte, error) {
	return c.execBulks(ctx, [][]byte{bs("HVALS"), key})
}

// --- List (§3.3) --------------------------------------------------------

// LPush prepends values, returning the new length.
func (c *Client) LPush(ctx context.Context, key []byte, values ...[]byte) (int64, error) {
	return c.execCount(ctx, prepend2("LPUSH", key, values))
}

// RPush appends values, returning the new length.
func (c *Client) RPush(ctx context.Context, key []byte, values ...[]byte) (int64, error) {
	return c.execCount(ctx, prepend2("RPUSH", key, values))
}

// LPop pops up to count values from the head (empty if drained).
func (c *Client) LPop(ctx context.Context, key []byte, count int) ([][]byte, error) {
	return c.listPop(ctx, "LPOP", key, count)
}

// RPop pops up to count values from the tail.
func (c *Client) RPop(ctx context.Context, key []byte, count int) ([][]byte, error) {
	return c.listPop(ctx, "RPOP", key, count)
}

func (c *Client) listPop(ctx context.Context, verb string, key []byte, count int) ([][]byte, error) {
	r, err := c.exec(ctx, [][]byte{bs(verb), key, bs(strconv.Itoa(count))})
	if err != nil {
		return nil, err
	}
	switch r.Kind {
	case KindArray:
		return arrayToBulks(r.Array)
	case KindBulk:
		return [][]byte{r.Bytes}, nil
	case KindNil, KindNull:
		return [][]byte{}, nil
	case KindError:
		return nil, replyErr(r)
	default:
		return nil, unexpectedReply(r)
	}
}

// LLen returns the list length (0 if absent).
func (c *Client) LLen(ctx context.Context, key []byte) (int64, error) {
	return c.execCount(ctx, [][]byte{bs("LLEN"), key})
}

// LRange returns elements start..stop with Redis negative indexing.
func (c *Client) LRange(ctx context.Context, key []byte, start, stop int64) ([][]byte, error) {
	return c.execBulks(ctx, [][]byte{bs("LRANGE"), key, itob(start), itob(stop)})
}

// --- Set (§3.4) ---------------------------------------------------------

// SAdd adds members, returning the newly-added count.
func (c *Client) SAdd(ctx context.Context, key []byte, members ...[]byte) (int64, error) {
	return c.execCount(ctx, prepend2("SADD", key, members))
}

// SRem removes members, returning the removed count.
func (c *Client) SRem(ctx context.Context, key []byte, members ...[]byte) (int64, error) {
	return c.execCount(ctx, prepend2("SREM", key, members))
}

// SMembers returns the members (order implementation-defined).
func (c *Client) SMembers(ctx context.Context, key []byte) ([][]byte, error) {
	return c.execBulks(ctx, [][]byte{bs("SMEMBERS"), key})
}

// SCard returns the member count (0 if absent).
func (c *Client) SCard(ctx context.Context, key []byte) (int64, error) {
	return c.execCount(ctx, [][]byte{bs("SCARD"), key})
}

// SIsMember reports membership.
func (c *Client) SIsMember(ctx context.Context, key, member []byte) (bool, error) {
	return c.execBool(ctx, [][]byte{bs("SISMEMBER"), key, member})
}

// SInter returns the intersection of the given sets.
func (c *Client) SInter(ctx context.Context, keys ...[]byte) ([][]byte, error) {
	return c.execBulks(ctx, prepend("SINTER", keys))
}

// SUnion returns the union of the given sets.
func (c *Client) SUnion(ctx context.Context, keys ...[]byte) ([][]byte, error) {
	return c.execBulks(ctx, prepend("SUNION", keys))
}

// SDiff returns the first set minus the rest.
func (c *Client) SDiff(ctx context.Context, keys ...[]byte) ([][]byte, error) {
	return c.execBulks(ctx, prepend("SDIFF", keys))
}

// --- Sorted set (§3.5) --------------------------------------------------

// ZMember is a (score, member) pair for ZAdd.
type ZMember struct {
	Score  float64
	Member []byte
}

// ZAdd adds scored members, returning the newly-added count.
func (c *Client) ZAdd(ctx context.Context, key []byte, members ...ZMember) (int64, error) {
	argv := make([][]byte, 0, len(members)*2+2)
	argv = append(argv, bs("ZADD"), key)
	for _, m := range members {
		argv = append(argv, ftob(m.Score), m.Member)
	}
	return c.execCount(ctx, argv)
}

// ZRem removes members, returning the removed count.
func (c *Client) ZRem(ctx context.Context, key []byte, members ...[]byte) (int64, error) {
	return c.execCount(ctx, prepend2("ZREM", key, members))
}

// ZScore returns a member's score; ok is false when absent.
func (c *Client) ZScore(ctx context.Context, key, member []byte) (score float64, ok bool, err error) {
	r, err := c.exec(ctx, [][]byte{bs("ZSCORE"), key, member})
	if err != nil {
		return 0, false, err
	}
	switch r.Kind {
	case KindBulk:
		f, perr := parseFloat(r.Bytes)
		if perr != nil {
			return 0, false, perr
		}
		return f, true, nil
	case KindDouble:
		return r.Double, true, nil
	case KindNil, KindNull:
		return 0, false, nil
	case KindError:
		return 0, false, replyErr(r)
	default:
		return 0, false, unexpectedReply(r)
	}
}

// ZCard returns the member count (0 if absent).
func (c *Client) ZCard(ctx context.Context, key []byte) (int64, error) {
	return c.execCount(ctx, [][]byte{bs("ZCARD"), key})
}

// ZRange returns members start..stop in ascending score order (members
// only, no scores; Redis negative indexing).
func (c *Client) ZRange(ctx context.Context, key []byte, start, stop int64) ([][]byte, error) {
	return c.execBulks(ctx, [][]byte{bs("ZRANGE"), key, itob(start), itob(stop)})
}

// --- shared helpers -----------------------------------------------------

func (c *Client) execOptBulk(ctx context.Context, argv [][]byte) ([]byte, bool, error) {
	r, err := c.exec(ctx, argv)
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

func prepend2(verb string, key []byte, rest [][]byte) [][]byte {
	out := make([][]byte, 0, len(rest)+2)
	out = append(out, bs(verb), key)
	out = append(out, rest...)
	return out
}

func ftob(f float64) []byte {
	return []byte(strconv.FormatFloat(f, 'g', -1, 64))
}

func parseFloat(b []byte) (float64, error) {
	f, err := strconv.ParseFloat(string(b), 64)
	if err != nil {
		return 0, errProtocol("bad float reply: %s", string(b))
	}
	return f, nil
}

func parseUint(b []byte) (uint64, error) {
	n, err := strconv.ParseUint(string(b), 10, 64)
	if err != nil {
		return 0, errProtocol("bad integer reply: %s", string(b))
	}
	return n, nil
}
