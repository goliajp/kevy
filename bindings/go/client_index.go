package kevy

import (
	"context"
	"encoding/binary"
	"math"
)

// Declarative secondary indexes: IDX.* (contract §3.8). Remote-only — the
// embedded backend answers Unsupported (its wire face coerces query bounds
// through the index's declared type server-side, which the client can't
// replicate without the catalog). Two-layered: typed shortcuts plus *Raw
// argv passthroughs that keep every server capability reachable.

// IdxRow is one IDX.QUERY hit: the row key plus the indexed value's wire
// string form (contract §4.3).
type IdxRow struct {
	Key   []byte
	Value []byte
}

// IdxPage is one page of IDX.QUERY RANGE/EQ results (contract §4.4).
type IdxPage struct {
	// Cursor is nil when the scan is complete (wire "0"); otherwise pass
	// it back via IdxQueryRange to resume.
	Cursor []byte
	// Rows are the hits in (value, key) order.
	Rows []IdxRow
}

// IdxInfo is one IDX.LIST entry (contract §4.5).
type IdxInfo struct {
	Name    []byte
	Prefix  []byte
	Kind    string // "range" | "unique" | "text" | "ann" | "agg"
	State   string // "ready" | "building"
	Entries uint64
	Bytes   uint64
}

// Ranked is one (key, score/distance) result from MATCH / KNN.
type Ranked struct {
	Key   []byte
	Score float64
}

// IdxCreateRange declares a plain range index:
// IDX.CREATE name ON PREFIX prefix FIELD field TYPE ty KIND range.
func (c *Client) IdxCreateRange(ctx context.Context, name, prefix, field []byte, ty IdxType) error {
	return c.IdxCreateRaw(ctx, name, bs("ON"), bs("PREFIX"), prefix, bs("FIELD"), field, bs("TYPE"), ty.tag(), bs("KIND"), bs("range"))
}

// IdxCreateRaw is the IDX.CREATE argv passthrough (everything after the
// verb) — covers MAXMEM, ANN DIM/DISTANCE/M/EF, GROUPBY, etc.
func (c *Client) IdxCreateRaw(ctx context.Context, args ...[]byte) error {
	if _, err := c.requireRemote("IDX.CREATE"); err != nil {
		return err
	}
	return c.execOK(ctx, prepend("IDX.CREATE", args))
}

// IdxDrop drops an index, returning whether it existed.
func (c *Client) IdxDrop(ctx context.Context, name []byte) (bool, error) {
	if _, err := c.requireRemote("IDX.DROP"); err != nil {
		return false, err
	}
	return c.execBool(ctx, [][]byte{bs("IDX.DROP"), name})
}

// IdxList returns the declared indexes (unknown labels skipped).
func (c *Client) IdxList(ctx context.Context) ([]IdxInfo, error) {
	if _, err := c.requireRemote("IDX.LIST"); err != nil {
		return nil, err
	}
	r, err := c.exec(ctx, argv1("IDX.LIST"))
	if err != nil {
		return nil, err
	}
	switch r.Kind {
	case KindArray:
		out := make([]IdxInfo, 0, len(r.Array))
		for _, entry := range r.Array {
			info, perr := parseIdxInfo(entry)
			if perr != nil {
				return nil, perr
			}
			out = append(out, info)
		}
		return out, nil
	case KindError:
		return nil, replyErr(r)
	default:
		return nil, unexpectedReply(r)
	}
}

// IdxQueryRange runs IDX.QUERY name RANGE min max [LIMIT n] [CURSOR c] and
// returns one page in (value, key) order. Resume with IdxPage.Cursor.
func (c *Client) IdxQueryRange(ctx context.Context, name, min, max []byte, limit int, cursor []byte) (IdxPage, error) {
	args := [][]byte{bs("RANGE"), min, max, bs("LIMIT"), itob(int64(limit))}
	if cursor != nil {
		args = append(args, bs("CURSOR"), cursor)
	}
	r, err := c.idxQueryRaw(ctx, name, args...)
	if err != nil {
		return IdxPage{}, err
	}
	return parseIdxPage(r)
}

// IdxQueryEq runs IDX.QUERY name EQ value [LIMIT n].
func (c *Client) IdxQueryEq(ctx context.Context, name, value []byte, limit int) (IdxPage, error) {
	r, err := c.idxQueryRaw(ctx, name, bs("EQ"), value, bs("LIMIT"), itob(int64(limit)))
	if err != nil {
		return IdxPage{}, err
	}
	return parseIdxPage(r)
}

// IdxQueryMatch runs IDX.QUERY name MATCH text [LIMIT n] and returns BM25
// (key, score) hits, best first.
func (c *Client) IdxQueryMatch(ctx context.Context, name, text []byte, limit int) ([]Ranked, error) {
	r, err := c.idxQueryRaw(ctx, name, bs("MATCH"), text, bs("LIMIT"), itob(int64(limit)))
	if err != nil {
		return nil, err
	}
	return parseRanked(r)
}

// IdxQueryKnn runs IDX.QUERY name KNN vector [LIMIT k], encoding vector as
// an f32 little-endian blob, and returns (key, distance) nearest first.
func (c *Client) IdxQueryKnn(ctx context.Context, name []byte, vector []float32, k int) ([]Ranked, error) {
	blob := make([]byte, len(vector)*4)
	for i, f := range vector {
		binary.LittleEndian.PutUint32(blob[i*4:], math.Float32bits(f))
	}
	r, err := c.idxQueryRaw(ctx, name, bs("KNN"), blob, bs("LIMIT"), itob(int64(k)))
	if err != nil {
		return nil, err
	}
	return parseRanked(r)
}

// IdxQueryRaw is the IDX.QUERY argv passthrough returning the raw Reply —
// the escape hatch for COMPOSE / HYBRID / GROUPS / FIELDS and any future
// query shape.
func (c *Client) IdxQueryRaw(ctx context.Context, args ...[]byte) (Reply, error) {
	if _, err := c.requireRemote("IDX.QUERY"); err != nil {
		return Reply{}, err
	}
	r, err := c.exec(ctx, prepend("IDX.QUERY", args))
	if err != nil {
		return Reply{}, err
	}
	if r.Kind == KindError {
		return Reply{}, replyErr(r)
	}
	return r, nil
}

func (c *Client) idxQueryRaw(ctx context.Context, name []byte, args ...[]byte) (Reply, error) {
	full := make([][]byte, 0, len(args)+1)
	full = append(full, name)
	full = append(full, args...)
	return c.IdxQueryRaw(ctx, full...)
}

func parseIdxPage(r Reply) (IdxPage, error) {
	if r.Kind != KindArray || len(r.Array) != 2 {
		return IdxPage{}, errProtocol("IDX.QUERY page: expected [cursor, rows]")
	}
	var page IdxPage
	switch cur := r.Array[0]; cur.Kind {
	case KindBulk:
		if string(cur.Bytes) != "0" {
			page.Cursor = cur.Bytes
		}
	default:
		return IdxPage{}, unexpectedReply(cur)
	}
	flat := r.Array[1]
	if flat.Kind != KindArray {
		return IdxPage{}, errProtocol("IDX.QUERY page: rows not an array")
	}
	if len(flat.Array)%2 != 0 {
		return IdxPage{}, errProtocol("IDX.QUERY page: odd row pair")
	}
	for i := 0; i < len(flat.Array); i += 2 {
		k, v := flat.Array[i], flat.Array[i+1]
		if k.Kind != KindBulk || v.Kind != KindBulk {
			return IdxPage{}, errProtocol("IDX.QUERY page: non-bulk row pair")
		}
		page.Rows = append(page.Rows, IdxRow{Key: k.Bytes, Value: v.Bytes})
	}
	return page, nil
}

func parseRanked(r Reply) ([]Ranked, error) {
	if r.Kind != KindArray {
		return nil, unexpectedReply(r)
	}
	out := make([]Ranked, 0, len(r.Array))
	for _, row := range r.Array {
		if row.Kind != KindArray || len(row.Array) < 2 {
			return nil, errProtocol("IDX.QUERY ranked row: expected [key, score]")
		}
		key, score := row.Array[0], row.Array[1]
		if key.Kind != KindBulk {
			return nil, unexpectedReply(key)
		}
		s, err := scoreOf(score)
		if err != nil {
			return nil, err
		}
		out = append(out, Ranked{Key: key.Bytes, Score: s})
	}
	return out, nil
}

func parseIdxInfo(entry Reply) (IdxInfo, error) {
	if entry.Kind != KindArray {
		return IdxInfo{}, unexpectedReply(entry)
	}
	var info IdxInfo
	cells := entry.Array
	for i := 0; i+1 < len(cells); i += 2 {
		label, value := cells[i], cells[i+1]
		if label.Kind != KindBulk || value.Kind != KindBulk {
			continue
		}
		switch string(label.Bytes) {
		case "name":
			info.Name = value.Bytes
		case "prefix":
			info.Prefix = value.Bytes
		case "kind":
			info.Kind = value.Str()
		case "state":
			info.State = value.Str()
		case "entries":
			n, err := parseUint(value.Bytes)
			if err != nil {
				return IdxInfo{}, err
			}
			info.Entries = n
		case "bytes":
			n, err := parseUint(value.Bytes)
			if err != nil {
				return IdxInfo{}, err
			}
			info.Bytes = n
		default:
			// forward-compatible: skip labels this version doesn't know
		}
	}
	return info, nil
}
