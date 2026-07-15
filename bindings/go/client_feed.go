package kevy

import "context"

// Change feed / CDC: FEED.* (contract §3.10). Both backends serve the same
// cursor contract. The embedded backend is single-shard (0) and requires
// the store opened with feed enabled; this port's Connect does not enable
// embedded feed, so embedded feed_tail/read answer Unsupported (matching
// the mem:// case), while feed_shards is always 1.

// FeedFrame is one applied effect's argv at a stream offset (contract §4.6).
type FeedFrame struct {
	Offset uint64
	Argv   [][]byte
}

// FeedBatch is a run of frames plus the cursor to resume from (contract
// §4.7). Resume the next FeedRead from (Generation, NextOffset).
type FeedBatch struct {
	Generation uint64
	NextOffset uint64
	Frames     []FeedFrame
}

// FeedShards returns the number of change-feed shards (embedded: 1).
func (c *Client) FeedShards(ctx context.Context) (int64, error) {
	if c.emb != nil {
		return 1, nil
	}
	return c.execCount(ctx, argv1("FEED.SHARDS"))
}

// FeedTail returns the shard's current (generation, next_offset) cursor —
// where a fresh or resumed consumer begins.
func (c *Client) FeedTail(ctx context.Context, shard int) (generation, nextOffset uint64, err error) {
	if c.emb != nil {
		if shard != 0 {
			return 0, 0, errInvalidInput("embedded feed is single-shard: shard must be 0")
		}
		return 0, 0, errUnsupported("feed disabled: this port's embedded Connect does not enable the change feed")
	}
	r, err := c.exec(ctx, [][]byte{bs("FEED.TAIL"), itob(int64(shard))})
	if err != nil {
		return 0, 0, err
	}
	if r.Kind == KindArray && len(r.Array) == 2 && r.Array[0].Kind == KindInt && r.Array[1].Kind == KindInt {
		return uint64(r.Array[0].Int), uint64(r.Array[1].Int), nil
	}
	if r.Kind == KindError {
		return 0, 0, replyErr(r)
	}
	return 0, 0, unexpectedReply(r)
}

// FeedRead delivers up to count frames past the cursor (server default
// 256, hard cap 4096), optionally key-prefix-filtered (fail-open). An
// unservable cursor surfaces as a Protocol error starting "FEEDRESYNC
// <gen> <tail>"; a cursor ahead of the stream as "ERR feed cursor ahead
// of stream". count <= 0 uses the server default.
func (c *Client) FeedRead(ctx context.Context, shard int, generation, offset uint64, count int, prefixes ...[]byte) (FeedBatch, error) {
	if c.emb != nil {
		if shard != 0 {
			return FeedBatch{}, errInvalidInput("embedded feed is single-shard: shard must be 0")
		}
		return FeedBatch{}, errUnsupported("feed disabled: this port's embedded Connect does not enable the change feed")
	}
	argv := [][]byte{bs("FEED.READ"), itob(int64(shard)), utob(generation), utob(offset)}
	if count > 0 {
		argv = append(argv, bs("COUNT"), itob(int64(count)))
	}
	for _, p := range prefixes {
		argv = append(argv, bs("PREFIX"), p)
	}
	r, err := c.exec(ctx, argv)
	if err != nil {
		return FeedBatch{}, err
	}
	return parseFeedBatch(r)
}

func parseFeedBatch(r Reply) (FeedBatch, error) {
	if r.Kind == KindError {
		return FeedBatch{}, replyErr(r)
	}
	if r.Kind != KindArray || len(r.Array) != 3 {
		return FeedBatch{}, errProtocol("FEED.READ: expected [gen, next, frames]")
	}
	g, next, frames := r.Array[0], r.Array[1], r.Array[2]
	if g.Kind != KindInt || next.Kind != KindInt {
		return FeedBatch{}, errProtocol("FEED.READ: non-integer cursor")
	}
	if frames.Kind != KindArray {
		return FeedBatch{}, errProtocol("FEED.READ: frames not an array")
	}
	batch := FeedBatch{Generation: uint64(g.Int), NextOffset: uint64(next.Int)}
	for _, f := range frames.Array {
		frame, err := parseFeedFrame(f)
		if err != nil {
			return FeedBatch{}, err
		}
		batch.Frames = append(batch.Frames, frame)
	}
	return batch, nil
}

func parseFeedFrame(f Reply) (FeedFrame, error) {
	if f.Kind != KindArray || len(f.Array) != 2 {
		return FeedFrame{}, errProtocol("FEED.READ: frame shape != [offset, argv]")
	}
	off, argvRaw := f.Array[0], f.Array[1]
	if off.Kind != KindInt || argvRaw.Kind != KindArray {
		return FeedFrame{}, errProtocol("FEED.READ: frame shape != [offset, argv]")
	}
	argv := make([][]byte, 0, len(argvRaw.Array))
	for _, a := range argvRaw.Array {
		switch a.Kind {
		case KindBulk, KindSimple:
			argv = append(argv, a.Bytes)
		default:
			return FeedFrame{}, unexpectedReply(a)
		}
	}
	return FeedFrame{Offset: uint64(off.Int), Argv: argv}, nil
}

func utob(n uint64) []byte { return itob(int64(n)) }
