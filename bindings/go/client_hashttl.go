package kevy

import (
	"context"
	"time"
)

// Hash-field TTL: HEXPIRE / HPEXPIRE / HPERSIST / HTTL / HPTTL (Redis 7.4
// shape, contract §3.7). Per-field replies come back in request order.
// Available on both backends.

// HExpire sets per-field TTLs with whole-second precision (ttl truncated
// to seconds). Returns one HExpireCode per field.
func (c *Client) HExpire(ctx context.Context, key []byte, fields [][]byte, ttl time.Duration, cond HExpireCond) ([]HExpireCode, error) {
	if err := checkFields(fields); err != nil {
		return nil, err
	}
	secs := int64(ttl / time.Second)
	return c.hashTTLCodes(ctx, "HEXPIRE", key, itob(secs), cond, fields)
}

// HPExpire is the millisecond-precision variant of HExpire.
func (c *Client) HPExpire(ctx context.Context, key []byte, fields [][]byte, ttl time.Duration, cond HExpireCond) ([]HExpireCode, error) {
	if err := checkFields(fields); err != nil {
		return nil, err
	}
	return c.hashTTLCodes(ctx, "HPEXPIRE", key, itob(durationToMs(ttl)), cond, fields)
}

// HPersist clears per-field TTLs. Codes: -2 missing, -1 no TTL, 1 cleared.
func (c *Client) HPersist(ctx context.Context, key []byte, fields ...[]byte) ([]HExpireCode, error) {
	if err := checkFields(fields); err != nil {
		return nil, err
	}
	return c.hashTTLCodes(ctx, "HPERSIST", key, nil, CondAlways, fields)
}

// HTTL returns remaining TTL per field in seconds (-2 missing, -1 no TTL).
func (c *Client) HTTL(ctx context.Context, key []byte, fields ...[]byte) ([]int64, error) {
	if err := checkFields(fields); err != nil {
		return nil, err
	}
	return c.hashTTLInts(ctx, "HTTL", key, nil, fields)
}

// HPTTL returns remaining TTL per field in milliseconds.
func (c *Client) HPTTL(ctx context.Context, key []byte, fields ...[]byte) ([]int64, error) {
	if err := checkFields(fields); err != nil {
		return nil, err
	}
	return c.hashTTLInts(ctx, "HPTTL", key, nil, fields)
}

func checkFields(fields [][]byte) error {
	if len(fields) == 0 {
		return errInvalidInput("hash field-TTL verbs need at least one field")
	}
	return nil
}

// hashTTLArgv builds "verb key [arg] [cond] FIELDS n field…".
func hashTTLArgv(verb string, key, arg []byte, cond HExpireCond, fields [][]byte) [][]byte {
	argv := make([][]byte, 0, len(fields)+6)
	argv = append(argv, bs(verb), key)
	if arg != nil {
		argv = append(argv, arg)
	}
	if kw := cond.keyword(); kw != nil {
		argv = append(argv, kw)
	}
	argv = append(argv, bs("FIELDS"), itob(int64(len(fields))))
	argv = append(argv, fields...)
	return argv
}

func (c *Client) hashTTLInts(ctx context.Context, verb string, key, arg []byte, fields [][]byte) ([]int64, error) {
	r, err := c.exec(ctx, hashTTLArgv(verb, key, arg, CondAlways, fields))
	if err != nil {
		return nil, err
	}
	switch r.Kind {
	case KindArray:
		out := make([]int64, 0, len(r.Array))
		for _, it := range r.Array {
			if it.Kind != KindInt {
				return nil, unexpectedReply(it)
			}
			out = append(out, it.Int)
		}
		return out, nil
	case KindError:
		return nil, replyErr(r)
	default:
		return nil, unexpectedReply(r)
	}
}

func (c *Client) hashTTLCodes(ctx context.Context, verb string, key, arg []byte, cond HExpireCond, fields [][]byte) ([]HExpireCode, error) {
	r, err := c.exec(ctx, hashTTLArgv(verb, key, arg, cond, fields))
	if err != nil {
		return nil, err
	}
	switch r.Kind {
	case KindArray:
		out := make([]HExpireCode, 0, len(r.Array))
		for _, it := range r.Array {
			if it.Kind != KindInt {
				return nil, unexpectedReply(it)
			}
			out = append(out, HExpireCode(it.Int))
		}
		return out, nil
	case KindError:
		return nil, replyErr(r)
	default:
		return nil, unexpectedReply(r)
	}
}
