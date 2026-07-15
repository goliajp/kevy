package kevy

import "context"

// Sorted-set algebra: ZINTERSTORE / ZUNIONSTORE / ZINTERCARD (contract
// §3.6). Available on both backends. AGGREGATE is emitted only for the
// non-default modes; WEIGHTS (when given) must be one-per-key.

// ZInterStore stores the unweighted-SUM intersection of keys into dest,
// returning dest's cardinality.
func (c *Client) ZInterStore(ctx context.Context, dest []byte, keys ...[]byte) (int64, error) {
	return c.ZInterStoreWith(ctx, dest, keys, nil, AggSum)
}

// ZInterStoreWith is the full-option intersection store.
func (c *Client) ZInterStoreWith(ctx context.Context, dest []byte, keys [][]byte, weights []float64, agg ZAggregate) (int64, error) {
	if err := checkSourceKeys(keys); err != nil {
		return 0, err
	}
	return c.execCount(ctx, zstoreArgv("ZINTERSTORE", dest, keys, weights, agg))
}

// ZUnionStore stores the unweighted-SUM union of keys into dest.
func (c *Client) ZUnionStore(ctx context.Context, dest []byte, keys ...[]byte) (int64, error) {
	return c.ZUnionStoreWith(ctx, dest, keys, nil, AggSum)
}

// ZUnionStoreWith is the full-option union store.
func (c *Client) ZUnionStoreWith(ctx context.Context, dest []byte, keys [][]byte, weights []float64, agg ZAggregate) (int64, error) {
	if err := checkSourceKeys(keys); err != nil {
		return 0, err
	}
	return c.execCount(ctx, zstoreArgv("ZUNIONSTORE", dest, keys, weights, agg))
}

// ZInterCard returns the intersection cardinality without materialising
// it. limit < 0 counts everything; limit >= 0 short-circuits at limit.
func (c *Client) ZInterCard(ctx context.Context, keys [][]byte, limit int) (int64, error) {
	if err := checkSourceKeys(keys); err != nil {
		return 0, err
	}
	argv := make([][]byte, 0, len(keys)+4)
	argv = append(argv, bs("ZINTERCARD"), itob(int64(len(keys))))
	argv = append(argv, keys...)
	if limit >= 0 {
		argv = append(argv, bs("LIMIT"), itob(int64(limit)))
	}
	return c.execCount(ctx, argv)
}

func checkSourceKeys(keys [][]byte) error {
	if len(keys) == 0 {
		return errInvalidInput("zset algebra needs at least one source key")
	}
	return nil
}

func zstoreArgv(verb string, dest []byte, keys [][]byte, weights []float64, agg ZAggregate) [][]byte {
	argv := make([][]byte, 0, len(keys)*2+6)
	argv = append(argv, bs(verb), dest, itob(int64(len(keys))))
	argv = append(argv, keys...)
	if weights != nil {
		argv = append(argv, bs("WEIGHTS"))
		for _, w := range weights {
			argv = append(argv, ftob(w))
		}
	}
	if agg != AggSum {
		argv = append(argv, bs("AGGREGATE"), agg.tag())
	}
	return argv
}
