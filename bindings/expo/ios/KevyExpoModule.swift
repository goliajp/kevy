// The Expo door, iOS side — a bare shell over the kevy-ffi C ABI (the
// same Kevy.xcframework KevyKit wraps; the podspec vendors it).
//
// Every function is synchronous JSI on the JS thread — the MMKV shape —
// so the handle maps below never see concurrent access. Handles are small
// ints, never raw pointers: a JS number is a double, and bits 53+ of a
// pointer would silently shear off. RESP stays opaque bytes here; parsing
// (and the whole typed surface) lives in src/index.ts, the same split as
// every other kevy door.

import ExpoModulesCore
import Kevy

public class KevyExpoModule: Module {
    private var dbs: [Int32: OpaquePointer] = [:]
    private var subs: [Int32: OpaquePointer] = [:]
    private var nextId: Int32 = 1

    private func claim() -> Int32 {
        defer { nextId += 1 }
        return nextId
    }

    private static func fail(_ msg: String) -> NSError {
        NSError(domain: "kevy", code: -1, userInfo: [NSLocalizedDescriptionKey: msg])
    }

    // The scalar GET lane can't carry a typed store error (GET on a non-string
    // key — its only error is WRONGTYPE). Throw with the KEVY_WRONGTYPE sentinel
    // so src/index.ts re-runs the framed GET and surfaces -ERR WRONGTYPE. This
    // mirrors Android's jp.golia.kevy.ScalarGetSignal.
    private static func wrongType() -> NSError {
        NSError(domain: "kevy", code: -2, userInfo: [
            NSLocalizedDescriptionKey: "KEVY_WRONGTYPE: scalar GET on a non-string key; use the framed GET",
        ])
    }

    public func definition() -> ModuleDefinition {
        Name("Kevy")

        Function("open") { (dir: String?) -> Int32 in
            let db: OpaquePointer?
            if let dir {
                let bytes = Array(dir.utf8)
                db = bytes.withUnsafeBufferPointer { kevy_open($0.baseAddress, $0.count) }
            } else {
                db = kevy_open_mem()
            }
            guard let db else { throw Self.fail("kevy: open failed") }
            let id = self.claim()
            self.dbs[id] = db
            return id
        }

        Function("close") { (id: Int32) in
            if let db = self.dbs.removeValue(forKey: id) {
                kevy_close(db)
            }
        }

        // Deterministic teardown: fsync all shards, then refuse later
        // writes (reads stay). Idempotent.
        Function("shutdown") { (id: Int32) in
            guard let db = self.dbs[id] else { throw Self.fail("kevy: closed handle") }
            guard kevy_shutdown(db) == 0 else {
                throw Self.fail("kevy: shutdown failed")
            }
        }

        // open with explicit durability/rewrite policy; opts is the
        // [fsync, shards, rewritePct, rewriteMinSize, rewriteBytes,
        // rewriteIntervalSecs] array the TS layer packs.
        Function("openWith") { (dir: String?, opts: [Double]) -> Int32 in
            var raw = KevyOpenOptions(
                fsync: UInt8(opts[0]), shards: UInt32(opts[1]),
                rewrite_pct: UInt32(opts[2]), rewrite_min_size: UInt64(opts[3]),
                rewrite_bytes: UInt64(opts[4]), rewrite_interval_secs: UInt64(opts[5]))
            let db: OpaquePointer?
            if let dir {
                let bytes = Array(dir.utf8)
                db = bytes.withUnsafeBufferPointer { kevy_open_with($0.baseAddress, $0.count, &raw) }
            } else {
                db = kevy_open_with(nil, 0, &raw)
            }
            guard let db else { throw Self.fail("kevy: open failed") }
            let id = self.claim()
            self.dbs[id] = db
            return id
        }

        Function("cmd") { (id: Int32, packed: Data) -> Data in
            guard let db = self.dbs[id] else { throw Self.fail("kevy: closed handle") }
            return try Self.runCmd(db, packed)
        }

        // The boot-replay verdict, as [replayedCommands, replayedBytes,
        // elapsedMs, droppedBytes, corrupt(0/1), quarantineCount] — the TS
        // layer maps it to a typed object. Mirrors the Android door's
        // KevyNative.openReport long[6] layout.
        Function("openReport") { (id: Int32) -> [Double] in
            guard let db = self.dbs[id] else { throw Self.fail("kevy: closed handle") }
            var rep = KevyOpenReport()
            guard kevy_open_report(db, &rep) == 0 else {
                throw Self.fail("kevy: open_report failed")
            }
            return [
                Double(rep.replayed_commands), Double(rep.replayed_bytes),
                Double(rep.elapsed_ms), Double(rep.dropped_bytes),
                rep.corrupt != 0 ? 1 : 0, Double(rep.quarantine_count),
            ]
        }

        // Rides the shared lane (kevy_get_shared) for cross-platform parity
        // with the Android door — NOT a measured speedup here: this shell is
        // already on the scalar (not RESP) path, so the shared lane's only
        // delta over plain kevy_get is one engine-side memcpy on large-bulk
        // hits, unmeasured on device. Free discipline: the shared lane's `cap`
        // is an OPAQUE owner handle, not a Vec capacity — it MUST be freed with
        // kevy_buf_free_shared, never kevy_buf_free (mixing the two frees is UB).
        Function("get") { (id: Int32, key: Data) -> Data? in
            guard let db = self.dbs[id] else { throw Self.fail("kevy: closed handle") }
            var out = KevyBuf()
            let rc = key.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                kevy_get_shared(db, raw.bindMemory(to: UInt8.self).baseAddress, raw.count, &out)
            }
            switch rc {
            case 1:
                defer { kevy_buf_free_shared(out.ptr, out.len, out.cap) }
                return Data(bytes: out.ptr, count: out.len)
            case 0:
                return nil
            case -2:
                throw Self.wrongType()
            default:
                throw Self.fail("kevy: kevy_get misuse")
            }
        }

        Function("set") { (id: Int32, key: Data, value: Data, ttlMs: Double) in
            guard let db = self.dbs[id] else { throw Self.fail("kevy: closed handle") }
            let rc = key.withUnsafeBytes { (k: UnsafeRawBufferPointer) in
                value.withUnsafeBytes { (v: UnsafeRawBufferPointer) in
                    kevy_set(
                        db,
                        k.bindMemory(to: UInt8.self).baseAddress, k.count,
                        v.bindMemory(to: UInt8.self).baseAddress, v.count,
                        UInt64(ttlMs > 0 ? ttlMs : 0))
                }
            }
            guard rc == 0 else { throw Self.fail("kevy: kevy_set misuse") }
        }

        Function("subscribe") { (id: Int32, chan: Data, pattern: Bool) -> Int32 in
            guard let db = self.dbs[id] else { throw Self.fail("kevy: closed handle") }
            let sub = chan.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> OpaquePointer? in
                let p = raw.bindMemory(to: UInt8.self).baseAddress
                return pattern
                    ? kevy_psubscribe(db, p, raw.count)
                    : kevy_subscribe(db, p, raw.count)
            }
            guard let sub else { throw Self.fail("kevy: subscribe failed") }
            let subId = self.claim()
            self.subs[subId] = sub
            return subId
        }

        Function("subNext") { (subId: Int32) -> Data? in
            guard let sub = self.subs[subId] else { throw Self.fail("kevy: closed handle") }
            var out = KevyBuf()
            let rc = kevy_sub_next(sub, &out)
            guard rc >= 0 else { throw Self.fail("kevy: subscription misuse") }
            if rc == 0 { return nil }
            defer { kevy_buf_free(out.ptr, out.len, out.cap) }
            return Data(bytes: out.ptr, count: out.len)
        }

        Function("subClose") { (subId: Int32) in
            if let sub = self.subs.removeValue(forKey: subId) {
                kevy_sub_close(sub)
            }
        }

        Function("version") { () -> String in
            String(cString: kevy_version())
        }
    }

    // Run one command from the packed argv form (u32-LE length prefix per
    // argument). The packed buffer is already one contiguous arena, so the
    // argv pointers just point into it — no per-argument copies.
    private static func runCmd(_ db: OpaquePointer, _ packed: Data) throws -> Data {
        var out = KevyBuf()
        let rc: Int32 = packed.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Int32 in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return -1 }
            var ptrs: [UnsafePointer<UInt8>?] = []
            var lens: [Int] = []
            var pos = 0
            while pos + 4 <= raw.count {
                let len = Int(base[pos]) | (Int(base[pos + 1]) << 8)
                    | (Int(base[pos + 2]) << 16) | (Int(base[pos + 3]) << 24)
                pos += 4
                guard pos + len <= raw.count else { return -1 }
                ptrs.append(base + pos)
                lens.append(len)
                pos += len
            }
            guard pos == raw.count, !ptrs.isEmpty else { return -1 }
            return kevy_cmd(db, ptrs.count, &ptrs, &lens, &out)
        }
        guard rc == 0 else { throw fail("kevy: kevy_cmd misuse") }
        defer { kevy_buf_free(out.ptr, out.len, out.cap) }
        return out.len > 0 ? Data(bytes: out.ptr, count: out.len) : Data()
    }
}
