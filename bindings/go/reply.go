package kevy

import (
	"strconv"
)

// ReplyKind tags one node of a decoded RESP reply. RESP2 uses the first
// six; RESP3 adds the rest (contract §4.1).
type ReplyKind int

const (
	// KindSimple is a +OK-style simple string.
	KindSimple ReplyKind = iota
	// KindError is a -ERR error frame (text without the leading '-').
	KindError
	// KindInt is a :N integer.
	KindInt
	// KindBulk is a $len bulk string.
	KindBulk
	// KindNil is a $-1 / *-1 RESP2 null.
	KindNil
	// KindArray is a *N array.
	KindArray
	// KindMap is a %N RESP3 map.
	KindMap
	// KindSet is a ~N RESP3 set.
	KindSet
	// KindDouble is a ,N RESP3 double.
	KindDouble
	// KindBoolean is a #t/#f RESP3 boolean.
	KindBoolean
	// KindVerbatim is a =N RESP3 verbatim string.
	KindVerbatim
	// KindBigNumber is a (N RESP3 big number (raw digit bytes).
	KindBigNumber
	// KindNull is a _ RESP3 true null.
	KindNull
	// KindPush is a >N RESP3 out-of-band push frame.
	KindPush
	// KindBlobError is a !N RESP3 length-prefixed error.
	KindBlobError
)

// ReplyPair is one key/value entry of a KindMap reply.
type ReplyPair struct {
	Key Reply
	Val Reply
}

// Reply is one decoded RESP value — the universal reply type covering all
// RESP2 and RESP3 shapes (contract §4.1).
type Reply struct {
	Kind ReplyKind
	// Bytes carries the payload for Simple / Error / Bulk / BigNumber /
	// BlobError, and the data body for Verbatim.
	Bytes []byte
	// Int carries the payload for Int.
	Int int64
	// Double carries the payload for Double.
	Double float64
	// Bool carries the payload for Boolean.
	Bool bool
	// VerbatimFmt is the 3-char format tag for Verbatim.
	VerbatimFmt [3]byte
	// Array carries the elements for Array / Set / Push.
	Array []Reply
	// Map carries the pairs for Map.
	Map []ReplyPair
}

// IsError reports whether the engine answered with a protocol error
// (KindError or KindBlobError).
func (r Reply) IsError() bool { return r.Kind == KindError || r.Kind == KindBlobError }

// IsNil reports whether the reply is a RESP2 nil or RESP3 null.
func (r Reply) IsNil() bool { return r.Kind == KindNil || r.Kind == KindNull }

// Str returns the payload as a string (Simple / Error / Bulk / Verbatim /
// BigNumber / BlobError), or "" for other kinds.
func (r Reply) Str() string { return string(r.Bytes) }

func (r Reply) shape() string {
	switch r.Kind {
	case KindSimple:
		return "simple-string"
	case KindError:
		return "error"
	case KindInt:
		return "integer"
	case KindBulk:
		return "bulk-string"
	case KindNil, KindNull:
		return "nil"
	case KindArray:
		return "array"
	case KindMap:
		return "map"
	case KindSet:
		return "set"
	case KindDouble:
		return "double"
	case KindBoolean:
		return "boolean"
	case KindVerbatim:
		return "verbatim-string"
	case KindBigNumber:
		return "big-number"
	case KindPush:
		return "push"
	case KindBlobError:
		return "blob-error"
	default:
		return "unknown"
	}
}

// errNeedMore is a sentinel: the buffer holds an incomplete frame.
var errNeedMore = errProtocol("incomplete RESP frame")

// decodeReply parses exactly one complete reply from a self-contained
// buffer (the FFI path, where the whole reply is present). It errors on a
// truncated or malformed frame.
func decodeReply(buf []byte) (Reply, error) {
	r, used, err := parseReply(buf)
	if err != nil {
		return Reply{}, err
	}
	if used == 0 {
		return Reply{}, errProtocol("truncated RESP reply")
	}
	return r, nil
}

// parseReply decodes one RESP reply from the front of buf. It returns the
// reply and the number of bytes consumed; a return of (_, 0, nil) means
// "need more bytes". A malformed frame returns a non-nil error. Speaks
// RESP2 + RESP3 and transparently skips attribute frames (contract §4.1).
func parseReply(buf []byte) (Reply, int, error) {
	if len(buf) == 0 {
		return Reply{}, 0, nil
	}
	switch buf[0] {
	case '+':
		return lineReply(buf, KindSimple)
	case '-':
		return lineReply(buf, KindError)
	case ':':
		return intReply(buf)
	case '$':
		return bulkReply(buf, KindBulk)
	case '*':
		return aggReply(buf, KindArray)
	case '%':
		return mapReply(buf)
	case '~':
		return aggReply(buf, KindSet)
	case ',':
		return doubleReply(buf)
	case '#':
		return boolReply(buf)
	case '=':
		return verbatimReply(buf)
	case '(':
		return lineReply(buf, KindBigNumber)
	case '_':
		return nullReply(buf)
	case '>':
		return aggReply(buf, KindPush)
	case '!':
		return bulkReply(buf, KindBlobError)
	case '|':
		return attributedReply(buf)
	default:
		return Reply{}, 0, errProtocol("unknown RESP tag %q", buf[0])
	}
}

// findCRLF returns the index of the CRLF terminating the payload that
// starts at from, or -1 if not yet present.
func findCRLF(buf []byte, from int) int {
	for i := from; i+1 < len(buf); i++ {
		if buf[i] == '\r' && buf[i+1] == '\n' {
			return i
		}
	}
	return -1
}

func lineReply(buf []byte, kind ReplyKind) (Reply, int, error) {
	eol := findCRLF(buf, 1)
	if eol < 0 {
		return Reply{}, 0, nil
	}
	return Reply{Kind: kind, Bytes: cloneBytes(buf[1:eol])}, eol + 2, nil
}

func intReply(buf []byte) (Reply, int, error) {
	eol := findCRLF(buf, 1)
	if eol < 0 {
		return Reply{}, 0, nil
	}
	n, err := strconv.ParseInt(string(buf[1:eol]), 10, 64)
	if err != nil {
		return Reply{}, 0, errProtocol("bad integer reply %q", buf[1:eol])
	}
	return Reply{Kind: KindInt, Int: n}, eol + 2, nil
}

func bulkReply(buf []byte, kind ReplyKind) (Reply, int, error) {
	hdr := findCRLF(buf, 1)
	if hdr < 0 {
		return Reply{}, 0, nil
	}
	n, err := strconv.Atoi(string(buf[1:hdr]))
	if err != nil {
		return Reply{}, 0, errProtocol("bad bulk length %q", buf[1:hdr])
	}
	if n < 0 {
		return Reply{Kind: KindNil}, hdr + 2, nil
	}
	start := hdr + 2
	end := start + n
	if len(buf) < end+2 {
		return Reply{}, 0, nil
	}
	return Reply{Kind: kind, Bytes: cloneBytes(buf[start:end])}, end + 2, nil
}

func aggReply(buf []byte, kind ReplyKind) (Reply, int, error) {
	hdr := findCRLF(buf, 1)
	if hdr < 0 {
		return Reply{}, 0, nil
	}
	count, err := strconv.Atoi(string(buf[1:hdr]))
	if err != nil {
		return Reply{}, 0, errProtocol("bad aggregate length %q", buf[1:hdr])
	}
	if count < 0 {
		if kind == KindPush {
			return Reply{}, 0, errProtocol("push frame cannot be null")
		}
		return Reply{Kind: KindNil}, hdr + 2, nil
	}
	pos := hdr + 2
	items := make([]Reply, 0, capHint(count, len(buf)-pos))
	for range count {
		item, used, perr := parseReply(buf[pos:])
		if perr != nil {
			return Reply{}, 0, perr
		}
		if used == 0 {
			return Reply{}, 0, nil
		}
		items = append(items, item)
		pos += used
	}
	return Reply{Kind: kind, Array: items}, pos, nil
}

func mapReply(buf []byte) (Reply, int, error) {
	hdr := findCRLF(buf, 1)
	if hdr < 0 {
		return Reply{}, 0, nil
	}
	count, err := strconv.Atoi(string(buf[1:hdr]))
	if err != nil || count < 0 {
		return Reply{}, 0, errProtocol("bad map length %q", buf[1:hdr])
	}
	pos := hdr + 2
	pairs := make([]ReplyPair, 0, capHint(count, (len(buf)-pos)/2))
	for range count {
		k, ku, kerr := parseReply(buf[pos:])
		if kerr != nil {
			return Reply{}, 0, kerr
		}
		if ku == 0 {
			return Reply{}, 0, nil
		}
		pos += ku
		v, vu, verr := parseReply(buf[pos:])
		if verr != nil {
			return Reply{}, 0, verr
		}
		if vu == 0 {
			return Reply{}, 0, nil
		}
		pos += vu
		pairs = append(pairs, ReplyPair{Key: k, Val: v})
	}
	return Reply{Kind: KindMap, Map: pairs}, pos, nil
}

func doubleReply(buf []byte) (Reply, int, error) {
	eol := findCRLF(buf, 1)
	if eol < 0 {
		return Reply{}, 0, nil
	}
	v, err := strconv.ParseFloat(string(buf[1:eol]), 64)
	if err != nil {
		return Reply{}, 0, errProtocol("bad double %q", buf[1:eol])
	}
	return Reply{Kind: KindDouble, Double: v}, eol + 2, nil
}

func boolReply(buf []byte) (Reply, int, error) {
	eol := findCRLF(buf, 1)
	if eol < 0 {
		return Reply{}, 0, nil
	}
	switch string(buf[1:eol]) {
	case "t":
		return Reply{Kind: KindBoolean, Bool: true}, eol + 2, nil
	case "f":
		return Reply{Kind: KindBoolean, Bool: false}, eol + 2, nil
	default:
		return Reply{}, 0, errProtocol("bad boolean payload")
	}
}

func verbatimReply(buf []byte) (Reply, int, error) {
	hdr := findCRLF(buf, 1)
	if hdr < 0 {
		return Reply{}, 0, nil
	}
	n, err := strconv.Atoi(string(buf[1:hdr]))
	if err != nil {
		return Reply{}, 0, errProtocol("bad verbatim length %q", buf[1:hdr])
	}
	if n < 4 {
		return Reply{}, 0, errProtocol("verbatim length < 4 (fmt + ':')")
	}
	start := hdr + 2
	end := start + n
	if len(buf) < end+2 {
		return Reply{}, 0, nil
	}
	body := buf[start:end]
	if body[3] != ':' {
		return Reply{}, 0, errProtocol("verbatim missing fmt:data separator")
	}
	var fmt3 [3]byte
	copy(fmt3[:], body[:3])
	return Reply{Kind: KindVerbatim, VerbatimFmt: fmt3, Bytes: cloneBytes(body[4:])}, end + 2, nil
}

func nullReply(buf []byte) (Reply, int, error) {
	if len(buf) < 3 {
		return Reply{}, 0, nil
	}
	if buf[0] != '_' || buf[1] != '\r' || buf[2] != '\n' {
		return Reply{}, 0, errProtocol("bad null payload")
	}
	return Reply{Kind: KindNull}, 3, nil
}

// attributedReply consumes a |N attribute map and returns the reply it
// decorates (contract §4.1: attributes are transparently discarded).
func attributedReply(buf []byte) (Reply, int, error) {
	_, used, err := mapReply(buf)
	if err != nil {
		return Reply{}, 0, err
	}
	if used == 0 {
		return Reply{}, 0, nil
	}
	r, u2, err := parseReply(buf[used:])
	if err != nil {
		return Reply{}, 0, err
	}
	if u2 == 0 {
		return Reply{}, 0, nil
	}
	return r, used + u2, nil
}

// capHint caps a header-declared count by the bytes remaining so a
// hostile length header cannot force a huge allocation (fuzz-hardened,
// mirrors the Rust parser).
func capHint(count, remaining int) int {
	if remaining < 0 {
		remaining = 0
	}
	if count > remaining {
		return remaining
	}
	return count
}

func cloneBytes(b []byte) []byte {
	if len(b) == 0 {
		return nil
	}
	out := make([]byte, len(b))
	copy(out, b)
	return out
}
