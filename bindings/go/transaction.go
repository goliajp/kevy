package kevy

import "context"

// Transactions: MULTI / EXEC / DISCARD + WATCH (contract §3.12).
// Remote-only — the embedded backend rejects multi/watch/unwatch with
// Unsupported (single-connection embedded access is already serial).
//
// Go has no deterministic destructors, so a Transaction abandoned without
// Exec*/Discard MUST be closed explicitly: call Close (or Discard) to send
// the implicit DISCARD and avoid leaving the socket in MULTI mode.
// Dropping a live Transaction without Close is a bug.

// Transaction is one in-flight MULTI block over a remote connection.
type Transaction struct {
	c    *respConn
	ctx  context.Context
	live bool
}

// Watch marks keys for optimistic concurrency; the next Multi on this
// connection aborts (EXEC returns Nil) if any watched key changed. Must
// precede Multi. Remote-only.
func (c *Client) Watch(ctx context.Context, keys ...[]byte) error {
	if len(keys) == 0 {
		return errInvalidInput("WATCH needs at least one key")
	}
	conn, err := c.requireRemote("WATCH")
	if err != nil {
		return err
	}
	return simpleOK(conn.request(ctx, prepend("WATCH", keys)))
}

// Unwatch drops every WATCH on this connection. Remote-only.
func (c *Client) Unwatch(ctx context.Context) error {
	conn, err := c.requireRemote("UNWATCH")
	if err != nil {
		return err
	}
	return simpleOK(conn.request(ctx, argv1("UNWATCH")))
}

// Multi starts a MULTI block. Remote-only.
func (c *Client) Multi(ctx context.Context) (*Transaction, error) {
	conn, err := c.requireRemote("MULTI/EXEC")
	if err != nil {
		return nil, err
	}
	if oerr := simpleOK(conn.request(ctx, argv1("MULTI"))); oerr != nil {
		return nil, oerr
	}
	return &Transaction{c: conn, ctx: ctx, live: true}, nil
}

// Queue queues one raw command (verb + args); expects +QUEUED.
func (t *Transaction) Queue(parts ...[]byte) error {
	if len(parts) == 0 {
		return errInvalidInput("Transaction.Queue needs at least a verb")
	}
	return t.queueArgv(parts)
}

// Typed chainable builders (each expects +QUEUED). They return the
// Transaction so calls can chain; the first error stops the chain.

// Set queues SET key value.
func (t *Transaction) Set(key, value []byte) (*Transaction, error) {
	return t.chain([][]byte{bs("SET"), key, value})
}

// Get queues GET key.
func (t *Transaction) Get(key []byte) (*Transaction, error) {
	return t.chain([][]byte{bs("GET"), key})
}

// Del queues DEL key [key ...].
func (t *Transaction) Del(keys ...[]byte) (*Transaction, error) {
	if len(keys) == 0 {
		return t, errInvalidInput("Transaction.Del needs at least one key")
	}
	return t.chain(prepend("DEL", keys))
}

// Exists queues EXISTS key [key ...].
func (t *Transaction) Exists(keys ...[]byte) (*Transaction, error) {
	if len(keys) == 0 {
		return t, errInvalidInput("Transaction.Exists needs at least one key")
	}
	return t.chain(prepend("EXISTS", keys))
}

// Incr queues INCR key.
func (t *Transaction) Incr(key []byte) (*Transaction, error) {
	return t.chain([][]byte{bs("INCR"), key})
}

// IncrBy queues INCRBY key delta.
func (t *Transaction) IncrBy(key []byte, delta int64) (*Transaction, error) {
	return t.chain([][]byte{bs("INCRBY"), key, itob(delta)})
}

// MGet queues MGET key [key ...].
func (t *Transaction) MGet(keys ...[]byte) (*Transaction, error) {
	if len(keys) == 0 {
		return t, errInvalidInput("Transaction.MGet needs at least one key")
	}
	return t.chain(prepend("MGET", keys))
}

// MSet queues MSET key value [key value ...].
func (t *Transaction) MSet(pairs ...[]byte) (*Transaction, error) {
	if len(pairs) == 0 || len(pairs)%2 != 0 {
		return t, errInvalidInput("Transaction.MSet needs a non-empty even key/value list")
	}
	return t.chain(prepend("MSET", pairs))
}

func (t *Transaction) chain(argv [][]byte) (*Transaction, error) {
	return t, t.queueArgv(argv)
}

func (t *Transaction) queueArgv(argv [][]byte) error {
	r, err := t.c.request(t.ctx, argv)
	if err != nil {
		return err
	}
	switch {
	case r.Kind == KindSimple && r.Str() == "QUEUED":
		return nil
	case r.Kind == KindError:
		return replyErr(r)
	default:
		return unexpectedReply(r)
	}
}

// Exec sends EXEC and returns the per-command replies. A WATCH abort (Nil)
// collapses to an empty slice (legacy); prefer ExecWatched for new code.
func (t *Transaction) Exec() ([]Reply, error) {
	r, err := t.finishExec()
	if err != nil {
		return nil, err
	}
	switch r.Kind {
	case KindArray:
		return r.Array, nil
	case KindNil, KindNull:
		return []Reply{}, nil
	case KindError:
		return nil, replyErr(r)
	default:
		return nil, unexpectedReply(r)
	}
}

// ExecWatched is Exec but ok is false when a WATCH violation aborted the
// transaction (Nil reply). ok true with an empty slice = empty success.
func (t *Transaction) ExecWatched() (replies []Reply, ok bool, err error) {
	r, ferr := t.finishExec()
	if ferr != nil {
		return nil, false, ferr
	}
	switch r.Kind {
	case KindArray:
		return r.Array, true, nil
	case KindNil, KindNull:
		return nil, false, nil
	case KindError:
		return nil, false, replyErr(r)
	default:
		return nil, false, unexpectedReply(r)
	}
}

// ExecTyped is Exec returning a typed reply cursor; a WATCH abort is a
// Protocol error.
func (t *Transaction) ExecTyped() (*TransactionReplies, error) {
	r, err := t.finishExec()
	if err != nil {
		return nil, err
	}
	switch r.Kind {
	case KindArray:
		return &TransactionReplies{items: r.Array}, nil
	case KindNil, KindNull:
		return nil, errProtocol("transaction aborted by WATCH")
	case KindError:
		return nil, replyErr(r)
	default:
		return nil, unexpectedReply(r)
	}
}

// ExecWatchedTyped is ExecTyped but ok is false on WATCH abort.
func (t *Transaction) ExecWatchedTyped() (cursor *TransactionReplies, ok bool, err error) {
	r, ferr := t.finishExec()
	if ferr != nil {
		return nil, false, ferr
	}
	switch r.Kind {
	case KindArray:
		return &TransactionReplies{items: r.Array}, true, nil
	case KindNil, KindNull:
		return nil, false, nil
	case KindError:
		return nil, false, replyErr(r)
	default:
		return nil, false, unexpectedReply(r)
	}
}

func (t *Transaction) finishExec() (Reply, error) {
	t.live = false
	return t.c.request(t.ctx, argv1("EXEC"))
}

// Discard abandons the queued commands.
func (t *Transaction) Discard() error {
	if !t.live {
		return nil
	}
	t.live = false
	return simpleOK(t.c.request(t.ctx, argv1("DISCARD")))
}

// Close sends an implicit DISCARD if the transaction is still live, so the
// socket isn't left in MULTI mode. Idempotent.
func (t *Transaction) Close() {
	if t.live {
		t.live = false
		_, _ = t.c.request(context.Background(), argv1("DISCARD"))
	}
}

func simpleOK(r Reply, err error) error {
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

// TransactionReplies is a typed cursor over a successful EXEC's replies
// (contract §4.10). Each Next* consumes one reply; a variant mismatch is a
// Protocol error and the cursor still advances so ExpectEmpty stays valid.
type TransactionReplies struct {
	items []Reply
	pos   int
}

// Remaining reports how many replies are un-consumed.
func (r *TransactionReplies) Remaining() int { return len(r.items) - r.pos }

// ExpectEmpty errors if any replies remain (an arity gate).
func (r *TransactionReplies) ExpectEmpty() error {
	if left := r.Remaining(); left != 0 {
		return errProtocol("transaction reply cursor has %d un-consumed replies", left)
	}
	return nil
}

// Raw pops the next reply as-is.
func (r *TransactionReplies) Raw() (Reply, error) {
	if r.pos >= len(r.items) {
		return Reply{}, errProtocol("exhausted")
	}
	v := r.items[r.pos]
	r.pos++
	return v, nil
}

// NextOK expects +OK.
func (r *TransactionReplies) NextOK() error {
	v, err := r.Raw()
	if err != nil {
		return err
	}
	if v.Kind == KindSimple && v.Str() == "OK" {
		return nil
	}
	return txMismatch("Simple(OK)", v)
}

// NextOKOrNil returns true for +OK, false for Nil (SET … NX/XX).
func (r *TransactionReplies) NextOKOrNil() (bool, error) {
	v, err := r.Raw()
	if err != nil {
		return false, err
	}
	switch {
	case v.Kind == KindSimple && v.Str() == "OK":
		return true, nil
	case v.IsNil():
		return false, nil
	default:
		return false, txMismatch("Simple(OK) or Nil", v)
	}
}

// NextInt expects an integer.
func (r *TransactionReplies) NextInt() (int64, error) {
	v, err := r.Raw()
	if err != nil {
		return 0, err
	}
	if v.Kind == KindInt {
		return v.Int, nil
	}
	return 0, txMismatch("Int", v)
}

// NextBulk expects a bulk (or Nil → nil,false).
func (r *TransactionReplies) NextBulk() (value []byte, ok bool, err error) {
	v, rerr := r.Raw()
	if rerr != nil {
		return nil, false, rerr
	}
	switch {
	case v.Kind == KindBulk:
		return v.Bytes, true, nil
	case v.IsNil():
		return nil, false, nil
	default:
		return nil, false, txMismatch("Bulk or Nil", v)
	}
}

// NextArrayOfBulks expects an array of Bulk/Nil (MGET), in order.
func (r *TransactionReplies) NextArrayOfBulks() ([][]byte, error) {
	v, err := r.Raw()
	if err != nil {
		return nil, err
	}
	switch v.Kind {
	case KindArray:
		out := make([][]byte, 0, len(v.Array))
		for _, it := range v.Array {
			switch {
			case it.Kind == KindBulk:
				out = append(out, it.Bytes)
			case it.IsNil():
				out = append(out, nil)
			default:
				return nil, txMismatch("Array element Bulk/Nil", it)
			}
		}
		return out, nil
	case KindNil, KindNull:
		return [][]byte{}, nil
	default:
		return nil, txMismatch("Array", v)
	}
}

// NextSimple expects any simple string (e.g. +PONG).
func (r *TransactionReplies) NextSimple() ([]byte, error) {
	v, err := r.Raw()
	if err != nil {
		return nil, err
	}
	if v.Kind == KindSimple {
		return v.Bytes, nil
	}
	return nil, txMismatch("Simple", v)
}

func txMismatch(want string, got Reply) *KevyError {
	return errProtocol("transaction reply mismatch: expected %s, got %s", want, got.shape())
}
