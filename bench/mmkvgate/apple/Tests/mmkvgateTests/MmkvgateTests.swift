// mmkvgate — kevy scalar fast path vs MMKV, XCTest.measure on iOS.
//
// Each test runs one engine on one axis (op × value size), so the Xcode
// test report reads as a head-to-head table. measure() reports the mean
// wall time of the block; each block does OPS operations, so per-op time
// is mean / OPS. kevy uses set/getData (kevy_set / kevy_get — no argv, no
// RESP); MMKV uses its synchronous mmap set/data. Warm keyspace (KEYS
// distinct keys, reused) unless a test says cold.
import XCTest
import KevyKit
import MMKV

private let OPS = 2000
private let KEYS = 200

private func payload(_ n: Int) -> Data { Data(repeating: 0x61, count: n) }

private func tmpDir(_ tag: String) -> String {
    let d = NSTemporaryDirectory() + "mmkvgate-\(tag)-\(UUID().uuidString)"
    try? FileManager.default.createDirectory(atPath: d, withIntermediateDirectories: true)
    return d
}

final class MmkvgateTests: XCTestCase {
    // ── kevy ────────────────────────────────────────────────────────────
    private func kevySet(_ size: Int) throws {
        let db = try KevyDB(dir: tmpDir("kevy-set"))
        let v = payload(size)
        measure { for i in 0..<OPS { try? db.set("k\(i % KEYS)", v) } }
    }
    private func kevyGet(_ size: Int) throws {
        let db = try KevyDB(dir: tmpDir("kevy-get"))
        let v = payload(size)
        for i in 0..<KEYS { try db.set("k\(i)", v) }
        measure { for i in 0..<OPS { _ = try? db.get("k\(i % KEYS)") } }
    }

    // ── MMKV ────────────────────────────────────────────────────────────
    private func mmkvInstance(_ tag: String) -> MMKV {
        MMKV.initialize(rootDir: tmpDir(tag))
        return MMKV(mmapID: "bench")!
    }
    private func mmkvSet(_ size: Int) {
        let m = mmkvInstance("mmkv-set")
        let v = payload(size)
        measure { for i in 0..<OPS { m.set(v, forKey: "k\(i % KEYS)") } }
    }
    private func mmkvGet(_ size: Int) {
        let m = mmkvInstance("mmkv-get")
        let v = payload(size)
        for i in 0..<KEYS { m.set(v, forKey: "k\(i)") }
        measure { for i in 0..<OPS { _ = m.data(forKey: "k\(i % KEYS)") } }
    }

    // ── the matrix: op × {16 B, 256 B, 4 KB} × {kevy, MMKV} ─────────────
    func testKevySet16()  throws { try kevySet(16) }
    func testMmkvSet16()        { mmkvSet(16) }
    func testKevyGet16()  throws { try kevyGet(16) }
    func testMmkvGet16()        { mmkvGet(16) }

    func testKevySet256() throws { try kevySet(256) }
    func testMmkvSet256()       { mmkvSet(256) }
    func testKevyGet256() throws { try kevyGet(256) }
    func testMmkvGet256()       { mmkvGet(256) }

    func testKevySet4K()  throws { try kevySet(4096) }
    func testMmkvSet4K()        { mmkvSet(4096) }
    func testKevyGet4K()  throws { try kevyGet(4096) }
    func testMmkvGet4K()        { mmkvGet(4096) }
}
