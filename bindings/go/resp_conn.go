package kevy

import (
	"context"
	"errors"
	"io"
	"net"
	"strconv"
	"time"
)

// respConn is a blocking RESP2/RESP3 connection over TCP — the native
// remote transport (contract §1.1). One request at a time; a read buffer
// reassembles multi-segment replies across reads. Not safe for concurrent
// use by multiple goroutines.
type respConn struct {
	conn    net.Conn
	rbuf    []byte
	wbuf    []byte
	hostStr string
}

// dialResp opens a TCP RESP connection with TCP_NODELAY.
func dialResp(host string, port uint16) (*respConn, error) {
	addr := net.JoinHostPort(host, strconv.Itoa(int(port)))
	conn, err := net.Dial("tcp", addr)
	if err != nil {
		return nil, errIo(err)
	}
	if tc, ok := conn.(*net.TCPConn); ok {
		_ = tc.SetNoDelay(true)
	}
	return &respConn{
		conn:    conn,
		rbuf:    make([]byte, 0, 8192),
		wbuf:    make([]byte, 0, 1024),
		hostStr: addr,
	}, nil
}

func (c *respConn) close() {
	if c.conn != nil {
		_ = c.conn.Close()
		c.conn = nil
	}
}

// encodeCommand writes argv as a RESP multibulk request.
func encodeCommand(out []byte, argv [][]byte) []byte {
	out = append(out, '*')
	out = strconv.AppendInt(out, int64(len(argv)), 10)
	out = append(out, '\r', '\n')
	for _, a := range argv {
		out = append(out, '$')
		out = strconv.AppendInt(out, int64(len(a)), 10)
		out = append(out, '\r', '\n')
		out = append(out, a...)
		out = append(out, '\r', '\n')
	}
	return out
}

// request sends one command and blocks until exactly one reply is parsed.
// ctx bounds the call: a deadline arms the socket, cancellation interrupts
// the in-flight read. A clean server close mid-read surfaces as Closed.
func (c *respConn) request(ctx context.Context, argv [][]byte) (Reply, error) {
	if c.conn == nil {
		return Reply{}, errClosed()
	}
	c.wbuf = encodeCommand(c.wbuf[:0], argv)
	replies, err := c.roundtrip(ctx, c.wbuf, 1)
	if err != nil {
		return Reply{}, err
	}
	return replies[0], nil
}

// pipelineRaw sends a pre-encoded batch as one write and reads n replies.
func (c *respConn) pipelineRaw(ctx context.Context, raw []byte, n int) ([]Reply, error) {
	if c.conn == nil {
		return nil, errClosed()
	}
	return c.roundtrip(ctx, raw, n)
}

func (c *respConn) roundtrip(ctx context.Context, wire []byte, n int) ([]Reply, error) {
	stop := c.armContext(ctx)
	defer stop()
	if err := ctxErr(ctx); err != nil {
		return nil, err
	}
	if _, err := c.conn.Write(wire); err != nil {
		return nil, c.mapIoErr(ctx, err)
	}
	out := make([]Reply, 0, n)
	for len(out) < n {
		r, err := c.readOne(ctx)
		if err != nil {
			return nil, err
		}
		out = append(out, r)
	}
	return out, nil
}

func (c *respConn) readOne(ctx context.Context) (Reply, error) {
	chunk := make([]byte, 8192)
	for {
		r, used, perr := parseReply(c.rbuf)
		if perr != nil {
			return Reply{}, errProtocol("malformed reply")
		}
		if used > 0 {
			c.rbuf = c.rbuf[used:]
			return r, nil
		}
		n, err := c.conn.Read(chunk)
		if n > 0 {
			c.rbuf = append(c.rbuf, chunk[:n]...)
			continue
		}
		if err != nil {
			return Reply{}, c.mapIoErr(ctx, err)
		}
	}
}

// armContext wires ctx cancellation/deadline onto the socket. On a
// deadline it sets the socket deadline; on cancellation a watcher
// interrupts the blocked read by pushing the deadline into the past
// (leaving the connection reusable). Returns a stop func.
func (c *respConn) armContext(ctx context.Context) func() {
	if ctx == nil {
		return func() {}
	}
	if dl, ok := ctx.Deadline(); ok {
		_ = c.conn.SetDeadline(dl)
	}
	done := ctx.Done()
	if done == nil {
		return func() { _ = c.conn.SetDeadline(time.Time{}) }
	}
	stopped := make(chan struct{})
	go func() {
		select {
		case <-done:
			_ = c.conn.SetReadDeadline(time.Now())
		case <-stopped:
		}
	}()
	return func() {
		close(stopped)
		_ = c.conn.SetDeadline(time.Time{})
	}
}

func (c *respConn) mapIoErr(ctx context.Context, err error) error {
	if errors.Is(err, io.EOF) {
		c.close()
		return errClosed()
	}
	var ne net.Error
	if errors.As(err, &ne) && ne.Timeout() {
		if ctx != nil && ctx.Err() != nil {
			if errors.Is(ctx.Err(), context.Canceled) {
				return &KevyError{Kind: KindClosed, Msg: "context canceled"}
			}
			return &KevyError{Kind: KindTimedOut}
		}
		return &KevyError{Kind: KindTimedOut}
	}
	c.close()
	return errIo(err)
}

func ctxErr(ctx context.Context) error {
	if ctx == nil {
		return nil
	}
	if err := ctx.Err(); err != nil {
		if errors.Is(err, context.DeadlineExceeded) {
			return &KevyError{Kind: KindTimedOut}
		}
		return &KevyError{Kind: KindClosed, Msg: "context canceled"}
	}
	return nil
}

// write sends argv as one command without reading a reply — the
// decoupled send side used by the pub/sub consumer, which then drains
// frames with readReply.
func (c *respConn) write(argv [][]byte) error {
	if c.conn == nil {
		return errClosed()
	}
	c.wbuf = encodeCommand(c.wbuf[:0], argv)
	if _, err := c.conn.Write(c.wbuf); err != nil {
		return c.mapIoErr(context.Background(), err)
	}
	return nil
}

// readReply blocks for the next reply frame, honouring ctx and any socket
// read deadline set via setReadDeadline. Used by the pub/sub consumer.
func (c *respConn) readReply(ctx context.Context) (Reply, error) {
	if c.conn == nil {
		return Reply{}, errClosed()
	}
	stop := c.armContext(ctx)
	defer stop()
	return c.readOne(ctx)
}

// setReadDeadline arms a bounded blocking read (or clears it with zero).
func (c *respConn) setReadDeadline(d time.Time) error {
	if c.conn == nil {
		return errClosed()
	}
	return c.conn.SetReadDeadline(d)
}

// selectDB issues a SELECT n round-trip; a -ERR reply fails the connect.
func (c *respConn) selectDB(db uint32) error {
	r, err := c.request(context.Background(), [][]byte{[]byte("SELECT"), []byte(strconv.FormatUint(uint64(db), 10))})
	if err != nil {
		return err
	}
	if r.Kind == KindError {
		return errProtocol("SELECT %d rejected: %s", db, r.Str())
	}
	return nil
}
