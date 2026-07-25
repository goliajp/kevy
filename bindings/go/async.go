package kevy

import "context"

// The async face (contract §1.4). ONE client exposes both faces: the
// blocking methods on *Client, and this *Async surface reached via
// Client.Async(). They AGREE because every async method delegates to the
// same blocking method on a goroutine and resolves a Future — so the async
// result is byte-for-byte the sync result.
//
// Go's idiom is blocking methods with context.Context; the async face is
// the goroutine-friendly future form the contract also requires. The typed
// async methods below cover the Rust async scope (string/generic +
// hash/list/set/zset + pipeline); every other command is reachable via the
// generic Async.Go runner and the raw Do future.

// Async is the async face over a Client.
type Async struct{ c *Client }

// Client returns the underlying blocking client (the two faces share it).
func (a *Async) Client() *Client { return a.c }

type asyncResult[T any] struct {
	val T
	err error
}

// Future is a pending async result. Await blocks for it (honouring ctx);
// Chan exposes the underlying channel for select-based composition.
type Future[T any] struct {
	ch chan asyncResult[T]
}

// Await blocks until the result is ready or ctx is done.
func (f *Future[T]) Await(ctx context.Context) (T, error) {
	select {
	case r := <-f.ch:
		return r.val, r.err
	case <-ctx.Done():
		var zero T
		return zero, ctxErr(ctx)
	}
}

// Chan returns a receive channel delivering the single result.
func (f *Future[T]) Chan() <-chan asyncResult[T] { return f.ch }

// runFuture spawns fn on a goroutine and resolves a Future with its result.
func runFuture[T any](fn func() (T, error)) *Future[T] {
	ch := make(chan asyncResult[T], 1)
	go func() {
		v, err := fn()
		ch <- asyncResult[T]{val: v, err: err}
	}()
	return &Future[T]{ch: ch}
}

// GoAsync runs an arbitrary blocking client op on a goroutine, returning a
// Future — the generic escape hatch so every command has an async form.
func GoAsync[T any](a *Async, ctx context.Context, fn func(context.Context, *Client) (T, error)) *Future[T] {
	return runFuture(func() (T, error) { return fn(ctx, a.c) })
}

// Do is the async raw-argv escape hatch.
func (a *Async) Do(ctx context.Context, argv ...[]byte) *Future[Reply] {
	return runFuture(func() (Reply, error) { return a.c.Do(ctx, argv...) })
}

// --- core string / generic (§3.1) ---------------------------------------

// Ping asynchronously sends PING.
func (a *Async) Ping(ctx context.Context) *Future[struct{}] {
	return runFuture(func() (struct{}, error) { return struct{}{}, a.c.Ping(ctx) })
}

// Set asynchronously stores value under key.
func (a *Async) Set(ctx context.Context, key, value []byte) *Future[struct{}] {
	return runFuture(func() (struct{}, error) { return struct{}{}, a.c.Set(ctx, key, value) })
}

// GetResult carries a Get outcome (value + presence).
type GetResult struct {
	Value []byte
	OK    bool
}

// Get asynchronously fetches key.
func (a *Async) Get(ctx context.Context, key []byte) *Future[GetResult] {
	return runFuture(func() (GetResult, error) {
		v, ok, err := a.c.Get(ctx, key)
		return GetResult{Value: v, OK: ok}, err
	})
}

// Del asynchronously removes keys.
func (a *Async) Del(ctx context.Context, keys ...[]byte) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.Del(ctx, keys...) })
}

// Exists asynchronously counts keys present.
func (a *Async) Exists(ctx context.Context, keys ...[]byte) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.Exists(ctx, keys...) })
}

// Incr asynchronously adds 1 to key.
func (a *Async) Incr(ctx context.Context, key []byte) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.Incr(ctx, key) })
}

// IncrBy asynchronously adds delta to key.
func (a *Async) IncrBy(ctx context.Context, key []byte, delta int64) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.IncrBy(ctx, key, delta) })
}

// Publish asynchronously delivers message to channel.
func (a *Async) Publish(ctx context.Context, channel, message []byte) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.Publish(ctx, channel, message) })
}

// --- collections (§3.2–§3.5) --------------------------------------------

// HSet asynchronously sets hash field/value pairs.
func (a *Async) HSet(ctx context.Context, key []byte, pairs ...[]byte) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.HSet(ctx, key, pairs...) })
}

// HGet asynchronously fetches one hash field.
func (a *Async) HGet(ctx context.Context, key, field []byte) *Future[GetResult] {
	return runFuture(func() (GetResult, error) {
		v, ok, err := a.c.HGet(ctx, key, field)
		return GetResult{Value: v, OK: ok}, err
	})
}

// LPush asynchronously prepends list values.
func (a *Async) LPush(ctx context.Context, key []byte, values ...[]byte) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.LPush(ctx, key, values...) })
}

// RPush asynchronously appends list values.
func (a *Async) RPush(ctx context.Context, key []byte, values ...[]byte) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.RPush(ctx, key, values...) })
}

// SAdd asynchronously adds set members.
func (a *Async) SAdd(ctx context.Context, key []byte, members ...[]byte) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.SAdd(ctx, key, members...) })
}

// ZAdd asynchronously adds sorted-set members.
func (a *Async) ZAdd(ctx context.Context, key []byte, members ...ZMember) *Future[int64] {
	return runFuture(func() (int64, error) { return a.c.ZAdd(ctx, key, members...) })
}

// --- pipeline (§3.13) ---------------------------------------------------

// Pipeline asynchronously runs a pipelined batch.
func (a *Async) Pipeline(ctx context.Context, build func(*PipelineBuf)) *Future[[]Reply] {
	return runFuture(func() ([]Reply, error) { return a.c.Pipeline(ctx, build) })
}
