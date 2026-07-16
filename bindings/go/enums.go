package kevy

// Enums shared with the embedded store and the wire protocol. See the
// kevy client contract §4.13 / §4.2.

// ZAggregate is the AGGREGATE mode for ZINTERSTORE / ZUNIONSTORE.
// The default, AggSum, emits no wire tag (§3.6).
type ZAggregate int

const (
	// AggSum sums weighted scores (the default).
	AggSum ZAggregate = iota
	// AggMin keeps the minimum weighted score.
	AggMin
	// AggMax keeps the maximum weighted score.
	AggMax
)

func (a ZAggregate) tag() []byte {
	switch a {
	case AggMin:
		return []byte("MIN")
	case AggMax:
		return []byte("MAX")
	default:
		return []byte("SUM")
	}
}

// HExpireCond is the optional condition on the hash field-TTL verbs
// (§3.7). CondAlways emits no wire keyword.
type HExpireCond int

const (
	// CondAlways sets the deadline unconditionally.
	CondAlways HExpireCond = iota
	// CondNx sets only when the field has no TTL.
	CondNx
	// CondXx sets only when the field already has a TTL.
	CondXx
	// CondGt sets only when the new deadline is later.
	CondGt
	// CondLt sets only when the new deadline is earlier.
	CondLt
)

func (c HExpireCond) keyword() []byte {
	switch c {
	case CondNx:
		return []byte("NX")
	case CondXx:
		return []byte("XX")
	case CondGt:
		return []byte("GT")
	case CondLt:
		return []byte("LT")
	default:
		return nil
	}
}

// HExpireCode is a signed 8-bit per-field result code from the hash
// field-TTL verbs (§3.7). Kept an int, not a bool.
type HExpireCode = int8

// IdxType is the declared scalar type of a range index (§4.2).
type IdxType int

const (
	// IdxI64 is a signed 64-bit integer field (wire tag "i64").
	IdxI64 IdxType = iota
	// IdxF64 is a finite 64-bit float field (wire tag "f64").
	IdxF64
	// IdxStr is a raw-bytes, memcmp-ordered field (wire tag "str").
	IdxStr
)

func (t IdxType) tag() []byte {
	switch t {
	case IdxF64:
		return []byte("f64")
	case IdxStr:
		return []byte("str")
	default:
		return []byte("i64")
	}
}
