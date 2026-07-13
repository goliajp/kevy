package kevy

// The typed surface — the API most applications live on. Every method is a
// thin mapping onto Cmd/CmdBytes; the full 184-verb surface stays reachable
// through Cmd itself. Here, unlike Cmd, a protocol error IS a Go error:
// a typed call has exactly one meaning, so "-WRONGTYPE" is a failure of the
// call, not data to inspect.

import (
	"errors"
	"fmt"
	"strconv"
	"time"
)

func (d *DB) want(v Value, err error) (Value, error) {
	if err != nil {
		return v, err
	}
	if v.IsError() {
		return v, errors.New("kevy: " + v.Str)
	}
	return v, nil
}

// Set stores value under key, replacing whatever was there.
func (d *DB) Set(key string, value []byte) error {
	_, err := d.want(d.CmdBytes([]byte("SET"), []byte(key), value))
	return err
}

// SetEx stores value under key with a time-to-live.
func (d *DB) SetEx(key string, value []byte, ttl time.Duration) error {
	ms := strconv.FormatInt(ttl.Milliseconds(), 10)
	_, err := d.want(d.CmdBytes([]byte("SET"), []byte(key), value, []byte("PX"), []byte(ms)))
	return err
}

// Get fetches key. ok is false on a miss (or an expired key).
func (d *DB) Get(key string) (value []byte, ok bool, err error) {
	v, err := d.want(d.Cmd("GET", key))
	if err != nil || v.Kind == KindNil {
		return nil, false, err
	}
	return []byte(v.Str), true, nil
}

// Del removes keys, returning how many actually existed.
func (d *DB) Del(keys ...string) (int64, error) {
	args := append([]string{"DEL"}, keys...)
	v, err := d.want(d.Cmd(args...))
	return v.Int, err
}

// Incr adds 1 to the integer at key (0 if absent) and returns the result.
func (d *DB) Incr(key string) (int64, error) { return d.IncrBy(key, 1) }

// IncrBy adds delta to the integer at key and returns the result.
func (d *DB) IncrBy(key string, delta int64) (int64, error) {
	v, err := d.want(d.Cmd("INCRBY", key, strconv.FormatInt(delta, 10)))
	return v.Int, err
}

// Expire sets key's time-to-live. false when the key does not exist.
func (d *DB) Expire(key string, ttl time.Duration) (bool, error) {
	ms := strconv.FormatInt(ttl.Milliseconds(), 10)
	v, err := d.want(d.Cmd("PEXPIRE", key, ms))
	return v.Int == 1, err
}

// PTTL reports key's remaining life in milliseconds — Redis semantics:
// -1 = exists with no TTL, -2 = no such key.
func (d *DB) PTTL(key string) (int64, error) {
	v, err := d.want(d.Cmd("PTTL", key))
	return v.Int, err
}

// Keys lists keys matching a glob pattern. On a large keyspace prefer
// paging with Cmd("SCAN", …).
func (d *DB) Keys(pattern string) ([]string, error) {
	v, err := d.want(d.Cmd("KEYS", pattern))
	if err != nil {
		return nil, err
	}
	out := make([]string, len(v.Arr))
	for i, item := range v.Arr {
		out[i] = item.Str
	}
	return out, nil
}

// MGet fetches many keys at once; a missing key yields a nil slice.
func (d *DB) MGet(keys ...string) ([][]byte, error) {
	args := append([]string{"MGET"}, keys...)
	v, err := d.want(d.Cmd(args...))
	if err != nil {
		return nil, err
	}
	out := make([][]byte, len(v.Arr))
	for i, item := range v.Arr {
		if item.Kind != KindNil {
			out[i] = []byte(item.Str)
		}
	}
	return out, nil
}

// DBSize counts live keys.
func (d *DB) DBSize() (int64, error) {
	v, err := d.want(d.Cmd("DBSIZE"))
	return v.Int, err
}

// FlushAll empties the store.
func (d *DB) FlushAll() error {
	_, err := d.want(d.Cmd("FLUSHALL"))
	return err
}

// Publish delivers payload to every in-process subscriber on channel,
// returning how many received it.
func (d *DB) Publish(channel string, payload []byte) (int64, error) {
	v, err := d.want(d.CmdBytes([]byte("PUBLISH"), []byte(channel), payload))
	return v.Int, err
}

// TypeOf reports the value kind at key: "string", "hash", "list", "set",
// "zset", or "none".
func (d *DB) TypeOf(key string) (string, error) {
	v, err := d.want(d.Cmd("TYPE", key))
	if err != nil {
		return "", err
	}
	if v.Str == "" {
		return "", fmt.Errorf("kevy: unexpected TYPE reply %+v", v)
	}
	return v.Str, nil
}
