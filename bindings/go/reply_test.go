package kevy

import (
	"math"
	"testing"
)

// RESP2 + RESP3 parser conformance (contract §4.1).

func TestParseRESP2(t *testing.T) {
	r := func(s string) Reply {
		v, used, err := parseReply([]byte(s))
		if err != nil || used == 0 {
			t.Fatalf("parse %q: used=%d err=%v", s, used, err)
		}
		return v
	}
	if got := r("+OK\r\n"); got.Kind != KindSimple || got.Str() != "OK" {
		t.Fatalf("simple: %+v", got)
	}
	if got := r("-ERR bad\r\n"); got.Kind != KindError || got.Str() != "ERR bad" {
		t.Fatalf("error: %+v", got)
	}
	if got := r(":42\r\n"); got.Kind != KindInt || got.Int != 42 {
		t.Fatalf("int: %+v", got)
	}
	if got := r("$5\r\nhello\r\n"); got.Kind != KindBulk || got.Str() != "hello" {
		t.Fatalf("bulk: %+v", got)
	}
	if got := r("$-1\r\n"); got.Kind != KindNil {
		t.Fatalf("nil bulk: %+v", got)
	}
	if got := r("*-1\r\n"); got.Kind != KindNil {
		t.Fatalf("nil array: %+v", got)
	}
	arr := r("*2\r\n:1\r\n$2\r\nhi\r\n")
	if arr.Kind != KindArray || len(arr.Array) != 2 || arr.Array[0].Int != 1 || arr.Array[1].Str() != "hi" {
		t.Fatalf("array: %+v", arr)
	}
}

func TestParseIncomplete(t *testing.T) {
	for _, s := range []string{"$5\r\nhel", "*2\r\n:1\r\n", "_", "#t", "=15\r\ntxt:Some"} {
		_, used, err := parseReply([]byte(s))
		if err != nil || used != 0 {
			t.Fatalf("incomplete %q: used=%d err=%v (want 0,nil)", s, used, err)
		}
	}
}

func TestParseRESP3(t *testing.T) {
	r := func(s string) Reply {
		v, _, err := parseReply([]byte(s))
		if err != nil {
			t.Fatalf("parse %q: %v", s, err)
		}
		return v
	}
	if r("_\r\n").Kind != KindNull {
		t.Fatal("null")
	}
	if got := r("#t\r\n"); got.Kind != KindBoolean || !got.Bool {
		t.Fatal("bool true")
	}
	if got := r(",1.5\r\n"); got.Kind != KindDouble || got.Double != 1.5 {
		t.Fatal("double")
	}
	if got := r(",inf\r\n"); !math.IsInf(got.Double, 1) {
		t.Fatal("double inf")
	}
	if got := r("(12345678901234567890\r\n"); got.Kind != KindBigNumber || got.Str() != "12345678901234567890" {
		t.Fatal("bignumber")
	}
	if got := r("!11\r\nERR bad cmd\r\n"); got.Kind != KindBlobError || got.Str() != "ERR bad cmd" {
		t.Fatal("bloberror")
	}
	verb := r("=15\r\ntxt:Some string\r\n")
	if verb.Kind != KindVerbatim || string(verb.VerbatimFmt[:]) != "txt" || verb.Str() != "Some string" {
		t.Fatalf("verbatim: %+v", verb)
	}
	m := r("%2\r\n:1\r\n$1\r\na\r\n:2\r\n$1\r\nb\r\n")
	if m.Kind != KindMap || len(m.Map) != 2 || m.Map[0].Val.Str() != "a" {
		t.Fatalf("map: %+v", m)
	}
	set := r("~3\r\n:1\r\n:2\r\n:3\r\n")
	if set.Kind != KindSet || len(set.Array) != 3 {
		t.Fatalf("set: %+v", set)
	}
	push := r(">3\r\n+message\r\n$4\r\nnews\r\n$5\r\nhello\r\n")
	if push.Kind != KindPush || len(push.Array) != 3 || push.Array[1].Str() != "news" {
		t.Fatalf("push: %+v", push)
	}
}

func TestParseAttributesSkipped(t *testing.T) {
	frame := "|1\r\n+key-popularity\r\n%2\r\n$1\r\na\r\n,0.5\r\n$1\r\nb\r\n,0.3\r\n*2\r\n:1\r\n:2\r\n"
	v, used, err := parseReply([]byte(frame))
	if err != nil {
		t.Fatal(err)
	}
	if v.Kind != KindArray || len(v.Array) != 2 || used != len(frame) {
		t.Fatalf("attributed: %+v used=%d", v, used)
	}
}
