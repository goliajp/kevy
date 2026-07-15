// Cluster-aware client (contract §3.15). One connection per shard, CRC16-slot
// routed so single-key commands hit their owner shard directly (no server
// -MOVED / forwarding hop). Requires the server in cluster mode. Topology
// discovered once at connect via CLUSTER SLOTS (16384 slots). CRC16 matches
// Redis's key_hash_slot exactly so routing agrees with the server.

import { replyText, toBytes, unexpectedReply, type Data, type Reply } from "./resp.ts";
import { ProtocolError } from "./errors.ts";
import { asBool, asCount, asInt, asOK, asOptBulk, av, replyError } from "./reply.ts";
import { RespConn } from "./transport.ts";
import { keyHashSlot } from "./crc16.ts";

const NUM_SLOTS = 16384;

interface Node {
  host: string;
  port: number;
}

/** One connection per distinct shard node + a slot→shard-index table. */
export class ClusterClient {
  #shards: RespConn[];
  #table: Uint16Array;
  private constructor(shards: RespConn[], table: Uint16Array) {
    this.#shards = shards;
    this.#table = table;
  }

  /** Connect via a seed node, discover topology, open one conn per shard. */
  static async connect(host: string, port: number): Promise<ClusterClient> {
    const seed = await RespConn.dial(host, port);
    let ranges: SlotRange[];
    try {
      ranges = parseClusterSlots(await seed.request(av("CLUSTER", "SLOTS")));
    } finally {
      seed.close();
    }
    const [nodes, table] = buildTopology(ranges);
    const shards: RespConn[] = [];
    try {
      for (const n of nodes) shards.push(await RespConn.dial(n.host, n.port));
    } catch (e) {
      for (const s of shards) s.close();
      throw e;
    }
    return new ClusterClient(shards, table);
  }

  close(): void {
    for (const s of this.#shards) s.close();
    this.#shards = [];
  }

  get shardCount(): number {
    return this.#shards.length;
  }

  #shardFor(key: Uint8Array): RespConn {
    return this.#shards[this.#table[keyHashSlot(key)]!]!;
  }

  /** Route a single-key command to the key's owner shard. */
  requestKeyed(key: Data, ...argv: Data[]): Promise<Reply> {
    return this.#shardFor(toBytes(key)).request(argv.map(toBytes));
  }
  /** Send a keyless command to shard 0. */
  requestUnkeyed(...argv: Data[]): Promise<Reply> {
    return this.#shards[0]!.request(argv.map(toBytes));
  }

  async ping(): Promise<void> {
    const r = await this.requestUnkeyed("PING");
    if (r.kind === "simple" && (replyText(r) === "PONG" || replyText(r) === "OK")) return;
    if (r.kind === "error") replyError(r);
    throw unexpectedReply(r);
  }
  publish(channel: Data, message: Data): Promise<number> {
    return this.requestUnkeyed("PUBLISH", channel, message).then(asCount);
  }

  set(key: Data, value: Data): Promise<void> {
    return this.requestKeyed(key, "SET", key, value).then(asOK);
  }
  setWithTtl(key: Data, value: Data, ttlMs: number): Promise<void> {
    return this.requestKeyed(key, "SET", key, value, "PX", String(Math.max(0, Math.floor(ttlMs)))).then(asOK);
  }
  get(key: Data): Promise<Uint8Array | null> {
    return this.requestKeyed(key, "GET", key).then(asOptBulk);
  }
  incr(key: Data): Promise<number> {
    return this.requestKeyed(key, "INCR", key).then(asInt);
  }
  incrBy(key: Data, delta: number): Promise<number> {
    return this.requestKeyed(key, "INCRBY", key, String(delta)).then(asInt);
  }
  expire(key: Data, ttlMs: number): Promise<boolean> {
    return this.requestKeyed(key, "PEXPIRE", key, String(Math.max(0, Math.floor(ttlMs)))).then(asBool);
  }
  persist(key: Data): Promise<boolean> {
    return this.requestKeyed(key, "PERSIST", key).then(asBool);
  }
  ttlMs(key: Data): Promise<number> {
    return this.requestKeyed(key, "PTTL", key).then(asInt);
  }

  /** Route each key to its owner and sum the removals (cross-shard OK). */
  del(...keys: Data[]): Promise<number> {
    return this.#perKeySum("DEL", keys);
  }
  exists(...keys: Data[]): Promise<number> {
    return this.#perKeySum("EXISTS", keys);
  }
  async #perKeySum(verb: string, keys: Data[]): Promise<number> {
    let total = 0;
    for (const k of keys) total += asInt(await this.requestKeyed(k, verb, k));
    return total;
  }

  dbsize(): Promise<number> {
    return this.requestUnkeyed("DBSIZE").then(asCount);
  }
  flushall(): Promise<void> {
    return this.requestUnkeyed("FLUSHALL").then(asOK);
  }
}

// --- CLUSTER SLOTS parsing + topology -----------------------------------

interface SlotRange {
  start: number;
  end: number;
  host: string;
  port: number;
}

function buildTopology(ranges: SlotRange[]): [Node[], Uint16Array] {
  if (ranges.length === 0) throw new ProtocolError("CLUSTER SLOTS returned no ranges");
  const nodes: Node[] = [];
  const table = new Uint16Array(NUM_SLOTS);
  for (const r of ranges) {
    let idx = nodes.findIndex((n) => n.host === r.host && n.port === r.port);
    if (idx < 0) {
      nodes.push({ host: r.host, port: r.port });
      idx = nodes.length - 1;
    }
    for (let slot = r.start; slot <= r.end; slot++) table[slot] = idx;
  }
  return [nodes, table];
}

function parseClusterSlots(reply: Reply): SlotRange[] {
  const bad = () => new ProtocolError("malformed CLUSTER SLOTS reply");
  if (reply.kind !== "array") throw bad();
  return reply.items.map((row) => {
    if (row.kind !== "array" || row.items.length < 3) throw bad();
    const start = asIntOpt(row.items[0]!);
    const end = asIntOpt(row.items[1]!);
    const node = row.items[2]!;
    if (start == null || end == null || node.kind !== "array" || node.items.length < 2) throw bad();
    const host = asStrOpt(node.items[0]!);
    const port = asIntOpt(node.items[1]!);
    if (host == null || port == null || !inU16(start) || !inU16(end) || !inU16(port)) throw bad();
    return { start, end, host, port };
  });
}

function asIntOpt(r: Reply): number | null {
  return r.kind === "int" ? r.value : null;
}
function asStrOpt(r: Reply): string | null {
  return r.kind === "bulk" || r.kind === "simple" ? replyText(r) : null;
}
function inU16(n: number): boolean {
  return n >= 0 && n <= 65535;
}
