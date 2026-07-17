// KevyKit — kevy embedded, Swift-shaped.
//
// The typed methods mirror the other kevy packages (set/get/del/incr/…);
// cmd() reaches all 184 verbs. Typed methods throw on a protocol error —
// a typed call has one meaning, so "-WRONGTYPE" is a failure. cmd()
// returns .error(…) as a VALUE for callers driving the raw verb surface.
//
//   let db = try KevyDB(dir: dataURL.path)
//   try db.set("session:7f3a", value, ttlMs: 3_600_000)
//   let v = try db.getText("session:7f3a")

import Foundation
import Kevy

/// One node of a parsed RESP reply.
public enum KevyValue: Equatable {
    case simple(String)
    case error(String)
    case int(Int64)
    case bulk(Data)
    case nilValue
    case array([KevyValue])

    public var isError: Bool {
        if case .error = self { return true }
        return false
    }

    /// Bulk / simple payload as UTF-8 text, when that is what it is.
    public var text: String? {
        switch self {
        case .simple(let s): return s
        case .bulk(let d): return String(data: d, encoding: .utf8)
        default: return nil
        }
    }

    public var int: Int64? {
        if case .int(let n) = self { return n }
        return nil
    }
}

/// A structured store-semantic error (contract §2.3): the engine rejected
/// the command because the key's stored type or value doesn't fit the verb.
/// Carried inside `KevyError.store`. Kept structured, never stringly.
public enum StoreError: Equatable {
    /// Key holds a different type than the command expects (Redis WRONGTYPE).
    case wrongType
    /// Any other recognized store error, wire text preserved verbatim.
    case other(String)
}

/// Errors thrown by the typed surface (never by `cmd`, which returns
/// protocol errors as values).
public enum KevyError: Error {
    case openFailed
    case misuse
    case store(StoreError)
    case protocolError(String)
    case truncatedReply
}

/// The embedded engine. One instance owns its data directory.
public final class KevyDB {
    private var db: OpaquePointer?

    /// Open a persistent store rooted at `dir`, replaying its log.
    public init(dir: String) throws {
        let bytes = Array(dir.utf8)
        db = bytes.withUnsafeBufferPointer { buf in
            kevy_open(buf.baseAddress, buf.count)
        }
        guard db != nil else { throw KevyError.openFailed }
    }

    /// Open a pure in-memory store — nothing survives the process.
    public init() throws {
        db = kevy_open_mem()
        guard db != nil else { throw KevyError.openFailed }
    }

    deinit { close() }

    /// Close the store. Safe to call more than once.
    public func close() {
        if let d = db {
            kevy_close(d)
            db = nil
        }
    }

    // ── the escape hatch: every verb, protocol errors as values ───────

    /// Run one command; `argv[0]` is the verb.
    public func cmd(_ argv: [String]) throws -> KevyValue {
        try cmdData(argv.map { Data($0.utf8) })
    }

    /// `cmd` for binary-safe arguments.
    public func cmdData(_ argv: [Data]) throws -> KevyValue {
        guard let d = db, !argv.isEmpty else { throw KevyError.misuse }
        // Manually-owned copies: a pointer must not outlive its
        // withUnsafeBufferPointer closure, and the argv array has to hold
        // all of them at once. The engine copies again on its side.
        var ptrs: [UnsafePointer<UInt8>?] = []
        var lens: [Int] = []
        var owned: [UnsafeMutablePointer<UInt8>] = []
        defer { owned.forEach { $0.deallocate() } }
        for a in argv {
            let p = UnsafeMutablePointer<UInt8>.allocate(capacity: max(a.count, 1))
            a.withUnsafeBytes { raw in
                if let base = raw.baseAddress, a.count > 0 {
                    p.update(from: base.assumingMemoryBound(to: UInt8.self), count: a.count)
                }
            }
            owned.append(p)
            ptrs.append(UnsafePointer(p))
            lens.append(a.count)
        }
        var out = KevyBuf()
        let rc = kevy_cmd(d, argv.count, &ptrs, &lens, &out)
        guard rc == 0 else { throw KevyError.misuse }
        return try Self.take(&out)
    }

    // ── the typed surface (mirrors the other kevy packages) ───────────

    /// Store `value` under `key`; `ttlMs` 0 means no expiry. Runs on the
    /// scalar `kevy_set` lane — no argv assembly, no RESP framing. The value
    /// bytes are passed straight from `Data`'s storage (no intermediate copy);
    /// the engine copies once on its side.
    public func set(_ key: String, _ value: Data, ttlMs: UInt64 = 0) throws {
        guard let d = db else { throw KevyError.misuse }
        let k = Array(key.utf8)
        let rc = k.withUnsafeBufferPointer { kb in
            value.withUnsafeBytes { raw -> Int32 in
                kevy_set(d, kb.baseAddress, kb.count,
                         raw.baseAddress?.assumingMemoryBound(to: UInt8.self),
                         raw.count, ttlMs)
            }
        }
        guard rc == 0 else { throw KevyError.misuse }
    }

    /// Fetch `key`; nil on a miss.
    ///
    /// Rides the zero-copy shared lane (`kevy_get_shared`, contract §5.2): a
    /// bulk value comes back as an engine `Arc` clone — a refcount bump, no
    /// byte copy — and the returned `Data` VIEWS those bytes with
    /// `bytesNoCopy`, pinning the Arc alive until the `Data` is freed (the
    /// MMKV-view analog). Relative to plain `kevy_get` + `Data(bytes:count:)`
    /// this drops the engine-side `Vec` copy and the client-side `Data` copy on
    /// large values — a memcpy elimination, so UNMEASURED on Swift and possibly
    /// throughput-neutral (methodology §8); adopted for the correct lane +
    /// parity with the siblings, not on a benched speedup claim. Needs a
    /// device/host bench before any speedup is stated.
    ///
    /// Shared-lane free discipline: a `kevy_get_shared` buffer MUST be released
    /// with `kevy_buf_free_shared` — never `kevy_buf_free` (that pairs with the
    /// RESP/`cmd` path, see `take`). `cap` is an OPAQUE owner handle (an Arc or
    /// a Vec), captured by the deallocator because Swift only hands it `ptr` +
    /// `len`. On the scalar lane's error (a negative rc — WRONGTYPE, GET on a
    /// non-string) fall back to the framed `cmd(["GET", key])` so the
    /// `-WRONGTYPE` reply surfaces as the typed `.store(.wrongType)`.
    public func get(_ key: String) throws -> Data? {
        guard let d = db else { throw KevyError.misuse }
        let k = Array(key.utf8)
        var out = KevyBuf()
        let rc = k.withUnsafeBufferPointer { kb in
            kevy_get_shared(d, kb.baseAddress, kb.count, &out)
        }
        switch rc {
        case 1:
            // Contract: ptr is null when len == 0 — free the owner handle and
            // return an owned-empty Data (never a Data over a null pointer).
            guard out.len > 0, let ptr = out.ptr else {
                kevy_buf_free_shared(out.ptr, out.len, out.cap)
                return Data()
            }
            let cap = out.cap
            let len = out.len
            return Data(bytesNoCopy: ptr, count: len, deallocator: .custom { p, _ in
                kevy_buf_free_shared(p, len, cap)
            })
        case 0:
            return nil
        default:
            // Scalar lane collapsed a store error into a code; re-issue framed
            // so a -WRONGTYPE surfaces as the typed error the siblings raise.
            return try getFramedFallback(key)
        }
    }

    /// Cold path behind `get`: run a framed `GET` so a store-semantic reply
    /// (WRONGTYPE) becomes the typed `.store` error instead of an opaque
    /// misuse (mirrors Go `Client.Get`, C++ `get_framed_fallback`).
    private func getFramedFallback(_ key: String) throws -> Data? {
        switch try cmd(["GET", key]) {
        case .bulk(let d): return d
        case .simple(let s): return Data(s.utf8)
        case .nilValue: return nil
        case .error(let msg): throw Self.replyError(msg)
        default: throw KevyError.protocolError("GET: unexpected reply")
        }
    }

    /// Fetch `key` as UTF-8 text; nil on a miss.
    public func getText(_ key: String) throws -> String? {
        try get(key).flatMap { String(data: $0, encoding: .utf8) }
    }

    /// Remove keys, returning how many existed.
    @discardableResult
    public func del(_ keys: String...) throws -> Int64 {
        try want(cmd(["DEL"] + keys)).int ?? 0
    }

    /// Add `delta` to the integer at `key`, returning the result.
    @discardableResult
    public func incr(_ key: String, by delta: Int64 = 1) throws -> Int64 {
        try want(cmd(["INCRBY", key, String(delta)])).int ?? 0
    }

    /// Set `key`'s time-to-live. `false` when the key does not exist.
    @discardableResult
    public func expire(_ key: String, ttlMs: Int64) throws -> Bool {
        try want(cmd(["PEXPIRE", key, String(ttlMs)])).int == 1
    }

    /// Remaining life in ms; Redis semantics (-1 no TTL, -2 no key).
    public func pttl(_ key: String) throws -> Int64 {
        try want(cmd(["PTTL", key])).int ?? -2
    }

    /// Keys matching a glob pattern. Page with cmd(["SCAN", …]) on a
    /// large keyspace.
    public func keys(_ pattern: String = "*") throws -> [String] {
        guard case .array(let items) = try want(cmd(["KEYS", pattern])) else { return [] }
        return items.compactMap(\.text)
    }

    /// Live key count.
    public func dbsize() throws -> Int64 {
        try want(cmd(["DBSIZE"])).int ?? 0
    }

    /// Empty the store.
    public func flushall() throws {
        _ = try want(cmd(["FLUSHALL"]))
    }

    /// Deliver `payload` to every in-process subscriber on `channel`.
    /// Returns the receiver count. Scalar lane (kevy_publish): no argv
    /// packing, no RESP reply to allocate and parse — the publish analog of
    /// the subscribe side, which has always been a direct symbol.
    @discardableResult
    public func publish(_ channel: String, _ payload: Data) throws -> Int64 {
        guard let d = db else { throw KevyError.misuse }
        let c = Array(channel.utf8)
        let n = c.withUnsafeBufferPointer { cb in
            payload.withUnsafeBytes { pb in
                kevy_publish(d, cb.baseAddress, cb.count,
                             pb.bindMemory(to: UInt8.self).baseAddress, pb.count)
            }
        }
        guard n >= 0 else { throw KevyError.misuse }
        return n
    }

    /// The boot-replay verdict: what this open restored — and what it
    /// could not. Non-zero `droppedBytes` / `corrupt` means the store
    /// recovered LESS than its files held (the dropped region was
    /// quarantined next to the AOF): surface it as a startup health check
    /// instead of scraping the boot WARN line from stderr.
    public struct OpenReport {
        public let replayedCommands: UInt64
        public let replayedBytes: UInt64
        public let elapsedMs: UInt64
        public let droppedBytes: UInt64
        public let corrupt: Bool
        public let quarantineCount: UInt32
    }

    public func openReport() throws -> OpenReport {
        guard let d = db else { throw KevyError.misuse }
        var rep = KevyOpenReport()
        guard kevy_open_report(d, &rep) == 0 else { throw KevyError.misuse }
        return OpenReport(
            replayedCommands: rep.replayed_commands,
            replayedBytes: rep.replayed_bytes,
            elapsedMs: rep.elapsed_ms,
            droppedBytes: rep.dropped_bytes,
            corrupt: rep.corrupt != 0,
            quarantineCount: rep.quarantine_count
        )
    }

    /// Open a polled subscription on one channel.
    public func subscribe(_ channel: String) throws -> KevySubscription {
        guard let d = db else { throw KevyError.misuse }
        let c = Array(channel.utf8)
        let s = c.withUnsafeBufferPointer { kevy_subscribe(d, $0.baseAddress, $0.count) }
        guard let s else { throw KevyError.misuse }
        return KevySubscription(s)
    }

    public static var version: String {
        String(cString: kevy_version())
    }

    // ── plumbing ───────────────────────────────────────────────────────

    private func want(_ v: KevyValue) throws -> KevyValue {
        if case .error(let msg) = v { throw Self.replyError(msg) }
        return v
    }

    /// Map a RESP `-ERR`/`-WRONGTYPE` reply body to the typed error: a
    /// recognized store-semantic reply becomes `.store` (WRONGTYPE the one the
    /// scalar lane can raise), everything else keeps its verbatim text as a
    /// `.protocolError` (contract §2.2). The leading "-" is already stripped.
    static func replyError(_ msg: String) -> KevyError {
        if msg.hasPrefix("WRONGTYPE") { return .store(.wrongType) }
        return .protocolError(msg)
    }

    static func take(_ out: inout KevyBuf) throws -> KevyValue {
        defer { kevy_buf_free(out.ptr, out.len, out.cap) }
        let bytes = UnsafeBufferPointer(start: out.ptr, count: out.len)
        var pos = 0
        return try parse(bytes, &pos)
    }

    private static func parse(_ b: UnsafeBufferPointer<UInt8>, _ pos: inout Int) throws -> KevyValue {
        func line() throws -> String {
            let start = pos
            while pos + 1 < b.count, !(b[pos] == 13 && b[pos + 1] == 10) { pos += 1 }
            guard pos + 1 < b.count else { throw KevyError.truncatedReply }
            let s = String(decoding: b[start..<pos], as: UTF8.self)
            pos += 2
            return s
        }
        guard pos < b.count else { throw KevyError.truncatedReply }
        let tag = b[pos]
        pos += 1
        switch tag {
        case UInt8(ascii: "+"): return .simple(try line())
        case UInt8(ascii: "-"): return .error(try line())
        case UInt8(ascii: ":"): return .int(Int64(try line()) ?? 0)
        case UInt8(ascii: "$"):
            let n = Int(try line()) ?? -1
            if n < 0 { return .nilValue }
            guard pos + n + 2 <= b.count else { throw KevyError.truncatedReply }
            let d = Data(b[pos..<(pos + n)])
            pos += n + 2
            return .bulk(d)
        case UInt8(ascii: "*"):
            let n = Int(try line()) ?? -1
            if n < 0 { return .nilValue }
            var items: [KevyValue] = []
            items.reserveCapacity(n)
            for _ in 0..<n { items.append(try parse(b, &pos)) }
            return .array(items)
        default:
            throw KevyError.truncatedReply
        }
    }
}

/// A polled subscription: drain with `next()`; deinit unsubscribes.
public final class KevySubscription {
    private var sub: OpaquePointer?

    init(_ s: OpaquePointer) { sub = s }
    deinit { close() }

    /// One pending frame (message / pmessage / ack); nil when drained.
    public func next() throws -> KevyValue? {
        guard let s = sub else { throw KevyError.misuse }
        var out = KevyBuf()
        let rc = kevy_sub_next(s, &out)
        if rc < 0 { throw KevyError.misuse }
        if rc == 0 { return nil }
        return try KevyDB.take(&out)
    }

    /// Block until one frame arrives or `timeoutMs` elapses (0 = wait forever).
    /// Parks the thread in the kernel — no busy-poll (contract §5.2) — so a
    /// push-style consumer can wait instead of spinning `next()`. Returns the
    /// frame, or nil on timeout.
    public func wait(timeoutMs: UInt64 = 0) throws -> KevyValue? {
        guard let s = sub else { throw KevyError.misuse }
        var out = KevyBuf()
        let rc = kevy_sub_wait(s, timeoutMs, &out)
        if rc < 0 { throw KevyError.misuse }
        if rc == 0 { return nil }
        return try KevyDB.take(&out)
    }

    public func close() {
        if let s = sub {
            kevy_sub_close(s)
            sub = nil
        }
    }
}

extension Array {
    fileprivate func inserting(_ e: Element, at i: Int) -> [Element] {
        var out = self
        out.insert(e, at: i)
        return out
    }
}
