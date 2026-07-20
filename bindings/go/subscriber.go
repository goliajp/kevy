package kevy

import (
	"context"
	"time"
)

// Pub/sub consumer side (contract §3.11). A subscribed connection cannot
// send normal commands, so the consumer is a distinct type. Publishing is
// done from a normal Client via Publish. Backends by URL: kevy:// /
// redis:// / tcp:// use a dedicated TCP socket; mem://<name> / file://
// use the in-process bus. Anonymous mem:// is rejected (no other
// producer — use a named bus).

// PubsubKind tags a PubsubEvent (contract §4.8).
type PubsubKind int

const (
	// EventSubscribe is a SUBSCRIBE ack.
	EventSubscribe PubsubKind = iota
	// EventPsubscribe is a PSUBSCRIBE ack.
	EventPsubscribe
	// EventUnsubscribe is an UNSUBSCRIBE ack (Channel nil = "all").
	EventUnsubscribe
	// EventPunsubscribe is a PUNSUBSCRIBE ack (Pattern nil = "all").
	EventPunsubscribe
	// EventMessage is a plain PUBLISH delivery.
	EventMessage
	// EventPmessage is a pattern-match delivery.
	EventPmessage
)

// PubsubEvent is one received pub/sub frame — acks and deliveries alike
// (contract §4.8). Count is the total channels + patterns held after the
// op. For Unsubscribe/Punsubscribe a nil Channel/Pattern means "all".
type PubsubEvent struct {
	Kind    PubsubKind
	Channel []byte
	Pattern []byte
	Payload []byte
	Count   int64
}

// Subscriber is one subscribed connection.
type Subscriber struct {
	remote      *respConn
	emb         *embSub
	readTimeout *time.Duration
	// pending holds events read while waiting for a subscribe ack, in
	// arrival order. See Subscribe.
	pending []PubsubEvent
}

// SubscriberConnect opens a fresh subscriber without subscribing yet
// (contract §3.11). Anonymous mem:// is rejected.
func SubscriberConnect(url string) (*Subscriber, error) {
	t, err := parseConnectURL(url)
	if err != nil {
		return nil, err
	}
	switch t.kind {
	case targetMemAnon:
		return nil, errUnsupported("anonymous mem:// has no other producer; use mem://<name> for a shared bus")
	case targetMemNamed, targetFile:
		db, key, oerr := resolveStore(t)
		if oerr != nil {
			return nil, oerr
		}
		return &Subscriber{emb: &embSub{db: db, key: key}}, nil
	default:
		conn, derr := dialResp(t.host, t.port)
		if derr != nil {
			return nil, derr
		}
		return &Subscriber{remote: conn}, nil
	}
}

// SubscriberConnectChannels connects and subscribes in one step (≥1
// channel required).
func SubscriberConnectChannels(url string, channels ...[]byte) (*Subscriber, error) {
	if len(channels) == 0 {
		return nil, errInvalidInput("SubscriberConnectChannels needs ≥ 1 channel — use SubscriberConnect for an empty start")
	}
	s, err := SubscriberConnect(url)
	if err != nil {
		return nil, err
	}
	if serr := s.Subscribe(channels...); serr != nil {
		s.Close()
		return nil, serr
	}
	return s, nil
}

// Subscribe subscribes to one or more channels.
func (s *Subscriber) Subscribe(channels ...[]byte) error {
	if len(channels) == 0 {
		return errInvalidInput("SUBSCRIBE needs ≥ 1 channel")
	}
	if s.remote != nil {
		if err := s.remote.write(prepend("SUBSCRIBE", channels)); err != nil {
			return err
		}
		return s.awaitAcks(len(channels), EventSubscribe)
	}
	return s.emb.add(channels, false)
}

// Psubscribe subscribes to one or more glob patterns.
func (s *Subscriber) Psubscribe(patterns ...[]byte) error {
	if len(patterns) == 0 {
		return errInvalidInput("PSUBSCRIBE needs ≥ 1 pattern")
	}
	if s.remote != nil {
		if err := s.remote.write(prepend("PSUBSCRIBE", patterns)); err != nil {
			return err
		}
		return s.awaitAcks(len(patterns), EventPsubscribe)
	}
	return s.emb.add(patterns, true)
}

// Unsubscribe unsubscribes from channels (empty = all).
func (s *Subscriber) Unsubscribe(channels ...[]byte) error {
	if s.remote != nil {
		return s.remote.write(prepend("UNSUBSCRIBE", channels))
	}
	s.emb.remove(channels, false)
	return nil
}

// Punsubscribe unsubscribes from patterns (empty = all).
func (s *Subscriber) Punsubscribe(patterns ...[]byte) error {
	if s.remote != nil {
		return s.remote.write(prepend("PUNSUBSCRIBE", patterns))
	}
	s.emb.remove(patterns, true)
	return nil
}

// Hello3 negotiates RESP3 push frames; remote-only, and must precede any
// subscribe (contract §3.11).
func (s *Subscriber) Hello3() (PubsubEvent, error) {
	if s.remote == nil {
		return PubsubEvent{}, errUnsupported("HELLO 3 is remote/TCP-only; the embedded bus has no proto switch")
	}
	if err := s.remote.write([][]byte{bs("HELLO"), bs("3")}); err != nil {
		return PubsubEvent{}, err
	}
	r, err := s.remote.readReply(context.Background())
	if err != nil {
		return PubsubEvent{}, err
	}
	switch r.Kind {
	case KindMap, KindArray:
		return PubsubEvent{Kind: EventSubscribe, Channel: bs("HELLO"), Count: 3}, nil
	case KindError:
		return PubsubEvent{}, replyErr(r)
	default:
		return PubsubEvent{}, errProtocol("unexpected HELLO 3 reply shape: %s", r.shape())
	}
}

// Recv blocks for the next pub/sub frame (acks and deliveries). A close
// surfaces as a Closed error; a read timeout as TimedOut.
// awaitAcks reads until n acks of kind have arrived, queueing everything
// else in arrival order.
//
// Subscribe used to write the command and return, handing back a
// subscriber that was not yet subscribed; publishing straight after raced
// the registration, and a lost message parks a blocking RecvMessage
// forever. Everything read while waiting is QUEUED, not consumed — a
// message on an already-subscribed channel can legitimately arrive before
// the ack for a new one — so the observable event stream is unchanged.
// See bench/FINDING-2026-07-19-subscribe-returns-before-live.md.
func (s *Subscriber) awaitAcks(n int, kind PubsubKind) error {
	for seen := 0; seen < n; {
		r, err := s.remote.readReply(context.Background())
		if err != nil {
			return err
		}
		ev, err := classifyFrame(r)
		if err != nil {
			return err
		}
		if ev.Kind == kind {
			seen++
		}
		s.pending = append(s.pending, ev)
	}
	return nil
}

func (s *Subscriber) Recv() (PubsubEvent, error) {
	if len(s.pending) > 0 {
		ev := s.pending[0]
		s.pending = s.pending[1:]
		return ev, nil
	}
	if s.remote != nil {
		r, err := s.remote.readReply(context.Background())
		if err != nil {
			return PubsubEvent{}, err
		}
		return classifyFrame(r)
	}
	r, err := s.emb.recv(s.readTimeout)
	if err != nil {
		return PubsubEvent{}, err
	}
	return classifyFrame(r)
}

// RecvMessage blocks for the next Message/Pmessage, skipping ack frames.
// For a pattern match Channel is the concrete channel (pattern discarded).
func (s *Subscriber) RecvMessage() (channel, payload []byte, err error) {
	for {
		ev, rerr := s.Recv()
		if rerr != nil {
			return nil, nil, rerr
		}
		switch ev.Kind {
		case EventMessage, EventPmessage:
			return ev.Channel, ev.Payload, nil
		default:
			// ack frame — keep waiting
		}
	}
}

// SetReadTimeout bounds a blocking Recv (nil clears it). A timeout
// surfaces as a TimedOut error.
func (s *Subscriber) SetReadTimeout(d *time.Duration) error {
	s.readTimeout = d
	if s.remote != nil {
		if d == nil {
			return s.remote.setReadDeadline(time.Time{})
		}
		return s.remote.setReadDeadline(time.Now().Add(*d))
	}
	return nil
}

// Close tears down the subscriber.
func (s *Subscriber) Close() {
	if s.remote != nil {
		s.remote.close()
		s.remote = nil
	}
	if s.emb != nil {
		s.emb.close()
		s.emb = nil
	}
}

// classifyFrame shapes a RESP2 array or RESP3 push frame into a
// PubsubEvent (contract §3.11: both delivery shapes accepted).
func classifyFrame(r Reply) (PubsubEvent, error) {
	var items []Reply
	switch r.Kind {
	case KindArray, KindPush:
		items = r.Array
	case KindError:
		return PubsubEvent{}, replyErr(r)
	default:
		return PubsubEvent{}, errProtocol("expected array/push frame, got %s", r.shape())
	}
	if len(items) == 0 || items[0].Kind != KindBulk {
		return PubsubEvent{}, errProtocol("pubsub frame missing kind field")
	}
	return shapeEvent(string(items[0].Bytes), items)
}

func shapeEvent(kind string, items []Reply) (PubsubEvent, error) {
	switch kind {
	case "subscribe":
		return ackEvent(EventSubscribe, items, false)
	case "psubscribe":
		return ackEvent(EventPsubscribe, items, true)
	case "unsubscribe":
		return ackEvent(EventUnsubscribe, items, false)
	case "punsubscribe":
		return ackEvent(EventPunsubscribe, items, true)
	case "message":
		if len(items) != 3 || items[1].Kind != KindBulk || items[2].Kind != KindBulk {
			return PubsubEvent{}, errProtocol("bad message frame")
		}
		return PubsubEvent{Kind: EventMessage, Channel: items[1].Bytes, Payload: items[2].Bytes}, nil
	case "pmessage":
		if len(items) != 4 || items[1].Kind != KindBulk || items[2].Kind != KindBulk || items[3].Kind != KindBulk {
			return PubsubEvent{}, errProtocol("bad pmessage frame")
		}
		return PubsubEvent{Kind: EventPmessage, Pattern: items[1].Bytes, Channel: items[2].Bytes, Payload: items[3].Bytes}, nil
	default:
		return PubsubEvent{}, errProtocol("unknown pubsub kind '%s'", kind)
	}
}

func ackEvent(kind PubsubKind, items []Reply, pattern bool) (PubsubEvent, error) {
	if len(items) != 3 {
		return PubsubEvent{}, errProtocol("expected 3-element pubsub frame, got %d", len(items))
	}
	var name []byte
	switch items[1].Kind {
	case KindBulk:
		name = items[1].Bytes
	case KindNil, KindNull:
		name = nil
	default:
		return PubsubEvent{}, errProtocol("bad pubsub name field")
	}
	if items[2].Kind != KindInt {
		return PubsubEvent{}, errProtocol("bad pubsub count field")
	}
	ev := PubsubEvent{Kind: kind, Count: items[2].Int}
	if pattern {
		ev.Pattern = name
	} else {
		ev.Channel = name
	}
	return ev, nil
}
