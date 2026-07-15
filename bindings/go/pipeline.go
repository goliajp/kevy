package kevy

import "context"

// Non-atomic pipeline batching (contract §3.13). Remote-only: the embedded
// backend answers Unsupported. Queue N commands client-side, send as one
// write, read N replies in order. NOT atomic — the server may interleave
// other clients. Per-command errors come back as Reply::Error entries
// INLINE (they do not abort the batch), unlike the typed single-command
// wraps.

// PipelineBuf accumulates commands for Client.Pipeline. An empty argv
// poisons the batch, surfaced as InvalidInput at send time.
type PipelineBuf struct {
	buf      []byte
	count    int
	poisoned bool
}

// Cmd queues one command (verb + args). Chainable; an empty parts poisons
// the batch.
func (p *PipelineBuf) Cmd(parts ...[]byte) *PipelineBuf {
	if len(parts) == 0 {
		p.poisoned = true
		return p
	}
	p.buf = encodeCommand(p.buf, parts)
	p.count++
	return p
}

// Len reports how many commands are queued.
func (p *PipelineBuf) Len() int { return p.count }

// IsEmpty reports whether nothing is queued.
func (p *PipelineBuf) IsEmpty() bool { return p.count == 0 }

// Pipeline runs a pipelined batch: build queues commands, then the batch
// is sent as one write and the per-command replies returned in queue
// order. An empty batch returns an empty slice without touching the wire;
// an empty-argv command returns InvalidInput. Remote-only.
func (c *Client) Pipeline(ctx context.Context, build func(*PipelineBuf)) ([]Reply, error) {
	conn, err := c.requireRemote("pipeline")
	if err != nil {
		return nil, err
	}
	var p PipelineBuf
	build(&p)
	if p.poisoned {
		return nil, errInvalidInput("pipeline: a queued command had an empty argv")
	}
	if p.count == 0 {
		return []Reply{}, nil
	}
	return conn.pipelineRaw(ctx, p.buf, p.count)
}
