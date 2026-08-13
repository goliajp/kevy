package kevy

import (
	"fmt"
	"testing"
	"time"
)

// Pub/sub round-trip (contract §3.11, §6). Named embedded bus + remote.

func pubsubBackends(t *testing.T) []backend {
	// Same rule as bothBackends: the embedded half exists only where the
	// engine was linked in.
	backends := []backend{}
	if openEmbeddedStore != nil {
		backends = append(backends, backend{
			name: "embedded",
			url: func(t *testing.T) (string, func()) {
				return fmt.Sprintf("mem://ps-%d", uniqueID()), func() {}
			},
		})
	}
	return append(backends, []backend{
		{
			name: "remote",
			url: func(t *testing.T) (string, func()) {
				s := spawnServer(t)
				return s.url(), func() {}
			},
		},
	}...)
}

func TestPubSubMessage(t *testing.T) {
	for _, bk := range pubsubBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()

			sub, err := SubscriberConnectChannels(url, b("news"))
			if err != nil {
				t.Fatal(err)
			}
			defer sub.Close()
			// First recv is the subscribe ack.
			ev, err := sub.Recv()
			if err != nil || ev.Kind != EventSubscribe {
				t.Fatalf("subscribe ack: %+v %v", ev, err)
			}

			pub := mustConnect(t, url)
			// Give the remote SUBSCRIBE time to register server-side.
			time.Sleep(50 * time.Millisecond)
			if _, perr := pub.Publish(bg, b("news"), b("hello")); perr != nil {
				t.Fatal(perr)
			}

			ch, payload, rerr := sub.RecvMessage()
			if rerr != nil || string(ch) != "news" || string(payload) != "hello" {
				t.Fatalf("RecvMessage=%q %q %v", ch, payload, rerr)
			}
		})
	}
}

func TestPubSubPattern(t *testing.T) {
	for _, bk := range pubsubBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()

			sub, err := SubscriberConnect(url)
			if err != nil {
				t.Fatal(err)
			}
			defer sub.Close()
			if err := sub.Psubscribe(b("ne*")); err != nil {
				t.Fatal(err)
			}
			ev, err := sub.Recv() // psubscribe ack
			if err != nil || ev.Kind != EventPsubscribe {
				t.Fatalf("psubscribe ack: %+v %v", ev, err)
			}

			pub := mustConnect(t, url)
			time.Sleep(50 * time.Millisecond)
			_, _ = pub.Publish(bg, b("news"), b("world"))

			ch, payload, rerr := sub.RecvMessage()
			if rerr != nil || string(ch) != "news" || string(payload) != "world" {
				t.Fatalf("pmessage=%q %q %v", ch, payload, rerr)
			}
		})
	}
}

func TestAnonymousMemSubscriberRejected(t *testing.T) {
	// Embedded-only: it opens the in-process engine directly.
	if openEmbeddedStore == nil {
		t.Skip("no embedded engine in this build")
	}
	_, err := SubscriberConnect("mem://")
	if !IsKind(err, KindUnsupported) {
		t.Fatalf("anonymous mem:// Subscriber should be Unsupported, got %v", err)
	}
}

func TestRemoteHello3PushFrames(t *testing.T) {
	s := spawnServer(t)
	sub, err := SubscriberConnect(s.url())
	if err != nil {
		t.Fatal(err)
	}
	defer sub.Close()
	// HELLO 3 must precede subscribe.
	if _, err := sub.Hello3(); err != nil {
		t.Fatalf("Hello3: %v", err)
	}
	if err := sub.Subscribe(b("rt")); err != nil {
		t.Fatal(err)
	}
	if ev, err := sub.Recv(); err != nil || ev.Kind != EventSubscribe {
		t.Fatalf("subscribe ack under RESP3: %+v %v", ev, err)
	}

	pub := mustConnect(t, s.url())
	time.Sleep(50 * time.Millisecond)
	_, _ = pub.Publish(bg, b("rt"), b("v3"))
	// recv handles RESP3 push (>N) transparently.
	ch, payload, rerr := sub.RecvMessage()
	if rerr != nil || string(ch) != "rt" || string(payload) != "v3" {
		t.Fatalf("RESP3 push delivery=%q %q %v", ch, payload, rerr)
	}
}

func TestEmbeddedHello3Unsupported(t *testing.T) {
	// Embedded-only: it opens the in-process engine directly.
	if openEmbeddedStore == nil {
		t.Skip("no embedded engine in this build")
	}
	sub, err := SubscriberConnect("mem://h3-bus")
	if err != nil {
		t.Fatal(err)
	}
	defer sub.Close()
	if _, err := sub.Hello3(); !IsKind(err, KindUnsupported) {
		t.Fatalf("embedded Hello3 should be Unsupported, got %v", err)
	}
}

func TestSubscriberReadTimeout(t *testing.T) {
	for _, bk := range pubsubBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			sub, err := SubscriberConnectChannels(url, b("quiet"))
			if err != nil {
				t.Fatal(err)
			}
			defer sub.Close()
			_, _ = sub.Recv() // drain ack
			d := 60 * time.Millisecond
			if err := sub.SetReadTimeout(&d); err != nil {
				t.Fatal(err)
			}
			start := time.Now()
			_, rerr := sub.Recv()
			if rerr == nil {
				t.Fatal("expected a timeout error on a quiet channel")
			}
			if time.Since(start) > 2*time.Second {
				t.Fatal("read timeout did not bound the blocking recv")
			}
		})
	}
}
