package kevy

import (
	"context"
	"time"
)

// Cluster-aware client (contract §3.15). One connection per shard,
// CRC16-slot routed so single-key commands hit their owner shard directly
// (no server -MOVED / forwarding hop). Requires the server in cluster
// mode. Topology discovered once at connect via CLUSTER SLOTS (16384
// slots). CRC16 matches Redis's key_hash_slot exactly so routing agrees
// with the server.

const numSlots = 16384

// ClusterClient holds one connection per distinct shard node plus a
// slot → shard-index table (contract §4.12).
type ClusterClient struct {
	shards     []*respConn
	slotToShrd []uint16
}

type slotRange struct {
	start, end uint16
	host       string
	port       uint16
}

// ClusterConnect connects via a seed node, discovers the topology, and
// opens one connection per shard.
func ClusterConnect(host string, port uint16) (*ClusterClient, error) {
	seed, err := dialResp(host, port)
	if err != nil {
		return nil, err
	}
	defer seed.close()
	reply, rerr := seed.request(context.Background(), [][]byte{bs("CLUSTER"), bs("SLOTS")})
	if rerr != nil {
		return nil, rerr
	}
	ranges, perr := parseClusterSlots(reply)
	if perr != nil {
		return nil, perr
	}
	nodes, table, terr := buildTopology(ranges)
	if terr != nil {
		return nil, terr
	}
	shards := make([]*respConn, 0, len(nodes))
	for _, n := range nodes {
		conn, derr := dialResp(n.host, n.port)
		if derr != nil {
			for _, s := range shards {
				s.close()
			}
			return nil, derr
		}
		shards = append(shards, conn)
	}
	return &ClusterClient{shards: shards, slotToShrd: table}, nil
}

// Close closes every shard connection.
func (cc *ClusterClient) Close() {
	for _, s := range cc.shards {
		s.close()
	}
	cc.shards = nil
}

// ShardCount reports the number of distinct shard nodes.
func (cc *ClusterClient) ShardCount() int { return len(cc.shards) }

func (cc *ClusterClient) shardFor(key []byte) *respConn {
	return cc.shards[cc.slotToShrd[KeyHashSlot(key)]]
}

// RequestKeyed routes a single-key command to the key's owner shard.
func (cc *ClusterClient) RequestKeyed(ctx context.Context, key []byte, argv ...[]byte) (Reply, error) {
	return cc.shardFor(key).request(ctx, argv)
}

// RequestUnkeyed sends a keyless command to shard 0.
func (cc *ClusterClient) RequestUnkeyed(ctx context.Context, argv ...[]byte) (Reply, error) {
	return cc.shards[0].request(ctx, argv)
}

// Ping is answered by any shard (accepts +PONG or +OK).
func (cc *ClusterClient) Ping(ctx context.Context) error {
	r, err := cc.RequestUnkeyed(ctx, bs("PING"))
	if err != nil {
		return err
	}
	switch {
	case r.Kind == KindSimple && (r.Str() == "PONG" || r.Str() == "OK"):
		return nil
	case r.Kind == KindError:
		return replyErr(r)
	default:
		return unexpectedReply(r)
	}
}

// Publish delivers to any shard (kevy pub/sub is process-global).
func (cc *ClusterClient) Publish(ctx context.Context, channel, message []byte) (int64, error) {
	return clusterCount(cc.RequestUnkeyed(ctx, bs("PUBLISH"), channel, message))
}

// Set stores value under key (key-routed).
func (cc *ClusterClient) Set(ctx context.Context, key, value []byte) error {
	return clusterOK(cc.RequestKeyed(ctx, key, bs("SET"), key, value))
}

// SetWithTTL is an atomic SET … PX ttl_ms (key-routed).
func (cc *ClusterClient) SetWithTTL(ctx context.Context, key, value []byte, ttl time.Duration) error {
	return clusterOK(cc.RequestKeyed(ctx, key, bs("SET"), key, value, bs("PX"), itob(durationToMs(ttl))))
}

// Get fetches key; ok false on miss (key-routed).
func (cc *ClusterClient) Get(ctx context.Context, key []byte) (value []byte, ok bool, err error) {
	r, rerr := cc.RequestKeyed(ctx, key, bs("GET"), key)
	if rerr != nil {
		return nil, false, rerr
	}
	switch r.Kind {
	case KindBulk:
		return r.Bytes, true, nil
	case KindNil, KindNull:
		return nil, false, nil
	case KindError:
		return nil, false, replyErr(r)
	default:
		return nil, false, unexpectedReply(r)
	}
}

// Incr adds 1 to key (key-routed).
func (cc *ClusterClient) Incr(ctx context.Context, key []byte) (int64, error) {
	return clusterInt(cc.RequestKeyed(ctx, key, bs("INCR"), key))
}

// IncrBy adds delta to key (key-routed).
func (cc *ClusterClient) IncrBy(ctx context.Context, key []byte, delta int64) (int64, error) {
	return clusterInt(cc.RequestKeyed(ctx, key, bs("INCRBY"), key, itob(delta)))
}

// Expire sets key's TTL (key-routed).
func (cc *ClusterClient) Expire(ctx context.Context, key []byte, ttl time.Duration) (bool, error) {
	return clusterBool(cc.RequestKeyed(ctx, key, bs("PEXPIRE"), key, itob(durationToMs(ttl))))
}

// Persist removes key's TTL (key-routed).
func (cc *ClusterClient) Persist(ctx context.Context, key []byte) (bool, error) {
	return clusterBool(cc.RequestKeyed(ctx, key, bs("PERSIST"), key))
}

// TTLMs returns ms remaining (key-routed).
func (cc *ClusterClient) TTLMs(ctx context.Context, key []byte) (int64, error) {
	return clusterInt(cc.RequestKeyed(ctx, key, bs("PTTL"), key))
}

// Del routes each key to its owner and sums the removals (cross-shard OK).
func (cc *ClusterClient) Del(ctx context.Context, keys ...[]byte) (int64, error) {
	return cc.perKeySum(ctx, "DEL", keys)
}

// Exists routes each key to its owner and sums the counts.
func (cc *ClusterClient) Exists(ctx context.Context, keys ...[]byte) (int64, error) {
	return cc.perKeySum(ctx, "EXISTS", keys)
}

func (cc *ClusterClient) perKeySum(ctx context.Context, verb string, keys [][]byte) (int64, error) {
	var total int64
	for _, k := range keys {
		n, err := clusterInt(cc.RequestKeyed(ctx, k, bs(verb), k))
		if err != nil {
			return 0, err
		}
		if n > 0 {
			total += n
		}
	}
	return total, nil
}

// DBSize returns the whole-cluster count (server fans out internally).
func (cc *ClusterClient) DBSize(ctx context.Context) (int64, error) {
	return clusterCount(cc.RequestUnkeyed(ctx, bs("DBSIZE")))
}

// FlushAll wipes the whole cluster (server fans out internally).
func (cc *ClusterClient) FlushAll(ctx context.Context) error {
	return clusterOK(cc.RequestUnkeyed(ctx, bs("FLUSHALL")))
}

// --- CLUSTER SLOTS parsing + topology -----------------------------------

func buildTopology(ranges []slotRange) ([]slotRange, []uint16, error) {
	if len(ranges) == 0 {
		return nil, nil, errProtocol("CLUSTER SLOTS returned no ranges")
	}
	var nodes []slotRange
	table := make([]uint16, numSlots)
	for _, r := range ranges {
		idx := -1
		for i, n := range nodes {
			if n.host == r.host && n.port == r.port {
				idx = i
				break
			}
		}
		if idx < 0 {
			nodes = append(nodes, slotRange{host: r.host, port: r.port})
			idx = len(nodes) - 1
		}
		for slot := int(r.start); slot <= int(r.end); slot++ {
			table[slot] = uint16(idx)
		}
	}
	return nodes, table, nil
}

func parseClusterSlots(reply Reply) ([]slotRange, error) {
	bad := func() error { return errProtocol("malformed CLUSTER SLOTS reply") }
	if reply.Kind != KindArray {
		return nil, bad()
	}
	out := make([]slotRange, 0, len(reply.Array))
	for _, row := range reply.Array {
		if row.Kind != KindArray || len(row.Array) < 3 {
			return nil, bad()
		}
		start, ok1 := asInt(row.Array[0])
		end, ok2 := asInt(row.Array[1])
		node := row.Array[2]
		if !ok1 || !ok2 || node.Kind != KindArray || len(node.Array) < 2 {
			return nil, bad()
		}
		host, ok3 := asStr(node.Array[0])
		port, ok4 := asInt(node.Array[1])
		if !ok3 || !ok4 || !inU16(start) || !inU16(end) || !inU16(port) {
			return nil, bad()
		}
		out = append(out, slotRange{start: uint16(start), end: uint16(end), host: host, port: uint16(port)})
	}
	return out, nil
}

func asInt(r Reply) (int64, bool) {
	if r.Kind == KindInt {
		return r.Int, true
	}
	return 0, false
}

func asStr(r Reply) (string, bool) {
	switch r.Kind {
	case KindBulk, KindSimple:
		return r.Str(), true
	default:
		return "", false
	}
}

func inU16(n int64) bool { return n >= 0 && n <= 65535 }

func clusterInt(r Reply, err error) (int64, error) {
	if err != nil {
		return 0, err
	}
	switch r.Kind {
	case KindInt:
		return r.Int, nil
	case KindError:
		return 0, replyErr(r)
	default:
		return 0, unexpectedReply(r)
	}
}

func clusterCount(r Reply, err error) (int64, error) {
	n, cerr := clusterInt(r, err)
	if cerr != nil {
		return 0, cerr
	}
	if n < 0 {
		return 0, errProtocol("expected non-negative count, got %d", n)
	}
	return n, nil
}

func clusterBool(r Reply, err error) (bool, error) {
	if err != nil {
		return false, err
	}
	switch r.Kind {
	case KindInt:
		return r.Int == 1, nil
	case KindError:
		return false, replyErr(r)
	default:
		return false, unexpectedReply(r)
	}
}

func clusterOK(r Reply, err error) error {
	if err != nil {
		return err
	}
	switch {
	case r.Kind == KindSimple && r.Str() == "OK":
		return nil
	case r.Kind == KindError:
		return replyErr(r)
	default:
		return unexpectedReply(r)
	}
}
