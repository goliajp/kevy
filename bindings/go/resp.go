package kevy

import (
	"errors"
	"fmt"
	"strconv"
)

// Kind tags one node of a parsed RESP reply.
type Kind int

// The RESP reply kinds kevy emits.
const (
	KindSimple Kind = iota // +OK
	KindError              // -ERR …
	KindInt                // :42
	KindBulk               // $3 foo
	KindNil                // $-1 / *-1
	KindArray              // *N …
)

// Value is one node of a reply tree.
type Value struct {
	Kind Kind
	Str  string  // Simple / Error / Bulk payload
	Int  int64   // Int payload
	Arr  []Value // Array items
}

// IsError reports whether the engine answered with a protocol error.
func (v Value) IsError() bool { return v.Kind == KindError }

// parseRESP decodes one complete reply, returning it and the bytes consumed.
func parseRESP(b []byte) (Value, int, error) {
	if len(b) == 0 {
		return Value{}, 0, errors.New("kevy: empty RESP")
	}
	line := func(from int) (string, int, error) {
		for i := from; i+1 < len(b); i++ {
			if b[i] == '\r' && b[i+1] == '\n' {
				return string(b[from:i]), i + 2, nil
			}
		}
		return "", 0, errors.New("kevy: truncated RESP line")
	}
	head, pos, err := line(1)
	if err != nil {
		return Value{}, 0, err
	}
	switch b[0] {
	case '+':
		return Value{Kind: KindSimple, Str: head}, pos, nil
	case '-':
		return Value{Kind: KindError, Str: head}, pos, nil
	case ':':
		n, err := strconv.ParseInt(head, 10, 64)
		if err != nil {
			return Value{}, 0, fmt.Errorf("kevy: bad integer %q", head)
		}
		return Value{Kind: KindInt, Int: n}, pos, nil
	case '$':
		n, err := strconv.Atoi(head)
		if err != nil {
			return Value{}, 0, fmt.Errorf("kevy: bad bulk length %q", head)
		}
		if n < 0 {
			return Value{Kind: KindNil}, pos, nil
		}
		if len(b) < pos+n+2 {
			return Value{}, 0, errors.New("kevy: truncated bulk")
		}
		return Value{Kind: KindBulk, Str: string(b[pos : pos+n])}, pos + n + 2, nil
	case '*':
		n, err := strconv.Atoi(head)
		if err != nil {
			return Value{}, 0, fmt.Errorf("kevy: bad array length %q", head)
		}
		if n < 0 {
			return Value{Kind: KindNil}, pos, nil
		}
		items := make([]Value, 0, n)
		for range n {
			item, used, err := parseRESP(b[pos:])
			if err != nil {
				return Value{}, 0, err
			}
			items = append(items, item)
			pos += used
		}
		return Value{Kind: KindArray, Arr: items}, pos, nil
	default:
		return Value{}, 0, fmt.Errorf("kevy: unknown RESP tag %q", b[0])
	}
}
