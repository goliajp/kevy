"""Pure Reply → typed parsers for the remote-only families (IDX.* §3.8,
FEED.* §3.10). Shared by the blocking and asyncio faces so both decode the
server's replies identically."""

from __future__ import annotations

import struct
from typing import List

from ._decode import raise_if_error, score_of, unexpected
from ._reply import Reply, ReplyKind
from ._types import FeedBatch, FeedFrame, IdxInfo, IdxPage, IdxRow, Ranked
from ._errors import ProtocolError


def knn_blob(vector: List[float]) -> bytes:
    """Serialize a query vector as a little-endian f32 blob (§7)."""
    return struct.pack("<" + "f" * len(vector), *vector)


def parse_idx_page(r: Reply) -> IdxPage:
    if r.is_error():
        raise_if_error(r)
    if r.kind is not ReplyKind.ARRAY or len(r.items) != 2:
        raise ProtocolError("IDX.QUERY page: expected [cursor, rows]")
    cur = r.items[0]
    cursor = None
    if cur.kind is ReplyKind.BULK:
        if cur.data != b"0":
            cursor = cur.data
    else:
        raise unexpected(cur)
    flat = r.items[1]
    if flat.kind is not ReplyKind.ARRAY:
        raise ProtocolError("IDX.QUERY page: rows not an array")
    if len(flat.items) % 2 != 0:
        raise ProtocolError("IDX.QUERY page: odd row pair")
    rows: List[IdxRow] = []
    for i in range(0, len(flat.items), 2):
        k, v = flat.items[i], flat.items[i + 1]
        if k.kind is not ReplyKind.BULK or v.kind is not ReplyKind.BULK:
            raise ProtocolError("IDX.QUERY page: non-bulk row pair")
        rows.append(IdxRow(key=k.data, value=v.data))
    return IdxPage(cursor=cursor, rows=rows)


def parse_ranked(r: Reply) -> List[Ranked]:
    if r.is_error():
        raise_if_error(r)
    if r.kind is not ReplyKind.ARRAY:
        raise unexpected(r)
    out: List[Ranked] = []
    for row in r.items:
        if row.kind is not ReplyKind.ARRAY or len(row.items) < 2:
            raise ProtocolError("IDX.QUERY ranked row: expected [key, score]")
        key, score = row.items[0], row.items[1]
        if key.kind is not ReplyKind.BULK:
            raise unexpected(key)
        out.append(Ranked(key=key.data, score=score_of(score)))
    return out


def parse_idx_info(entry: Reply) -> IdxInfo:
    if entry.kind is not ReplyKind.ARRAY:
        raise unexpected(entry)
    info = IdxInfo()
    cells = entry.items
    i = 0
    while i + 1 < len(cells):
        label, value = cells[i], cells[i + 1]
        i += 2
        if label.kind is not ReplyKind.BULK or value.kind is not ReplyKind.BULK:
            continue
        name = label.data
        if name == b"name":
            info.name = value.data
        elif name == b"prefix":
            info.prefix = value.data
        elif name == b"kind":
            info.kind = value.text()
        elif name == b"state":
            info.state = value.text()
        elif name == b"entries":
            info.entries = int(value.data)
        elif name == b"bytes":
            info.bytes_ = int(value.data)
        # else: forward-compatible — skip unknown labels
    return info


def parse_idx_list(r: Reply) -> List[IdxInfo]:
    if r.is_error():
        raise_if_error(r)
    if r.kind is not ReplyKind.ARRAY:
        raise unexpected(r)
    return [parse_idx_info(e) for e in r.items]


def parse_feed_batch(r: Reply) -> FeedBatch:
    if r.is_error():
        raise_if_error(r)
    if r.kind is not ReplyKind.ARRAY or len(r.items) != 3:
        raise ProtocolError("FEED.READ: expected [gen, next, frames]")
    g, nxt, frames = r.items
    if g.kind is not ReplyKind.INT or nxt.kind is not ReplyKind.INT:
        raise ProtocolError("FEED.READ: non-integer cursor")
    if frames.kind is not ReplyKind.ARRAY:
        raise ProtocolError("FEED.READ: frames not an array")
    batch = FeedBatch(generation=g.integer, next_offset=nxt.integer, frames=[])
    for f in frames.items:
        batch.frames.append(_parse_feed_frame(f))
    return batch


def _parse_feed_frame(f: Reply) -> FeedFrame:
    if f.kind is not ReplyKind.ARRAY or len(f.items) != 2:
        raise ProtocolError("FEED.READ: frame shape != [offset, argv]")
    off, argv_raw = f.items
    if off.kind is not ReplyKind.INT or argv_raw.kind is not ReplyKind.ARRAY:
        raise ProtocolError("FEED.READ: frame shape != [offset, argv]")
    argv: List[bytes] = []
    for a in argv_raw.items:
        if a.kind in (ReplyKind.BULK, ReplyKind.SIMPLE):
            argv.append(a.data)
        else:
            raise unexpected(a)
    return FeedFrame(offset=off.integer, argv=argv)
