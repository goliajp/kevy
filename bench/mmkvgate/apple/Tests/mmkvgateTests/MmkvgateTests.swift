// The axes. Warm = a small reused keyspace; cold set = every key new;
// reopened get = first read of each key after the store is reopened;
// batch = one call carrying many keys where the engine has one (kevy
// MSET / MGET; MMKV has no batch API, so its batch is the loop an app
// would write); open = open a store holding 10k keys and read one;
// close = close that store, whatever it waits for.
import XCTest

private let OPS = 2000
private let KEYS = 200
private let SIZES = [16, 256, 4096]

final class MmkvgateTests: XCTestCase {
    func testWarmGet() throws {
        for size in SIZES {
            let v = payload(size)
            let k = try KevyEngine("get"), m = MmkvEngine("get")
            for i in 0..<KEYS { try k.set("k\(i)", v); m.set("k\(i)", v) }
            let keys = (0..<OPS).map { "k\($0 % KEYS)" }
            try cell("get_warm", bytes: size, ops: OPS, kevy: { _ in
                try timeNs { for key in keys { let got = try k.get(key)?.count; precondition(got == size) } }
            }, mmkv: { _ in
                timeNs { for key in keys { precondition(m.get(key)?.count == size) } }
            })
        }
    }

    func testWarmSet() throws {
        for size in SIZES {
            let v = payload(size)
            let k = try KevyEngine("set"), m = MmkvEngine("set")
            let keys = (0..<OPS).map { "k\($0 % KEYS)" }
            try cell("set_warm", bytes: size, ops: OPS, kevy: { _ in
                try timeNs { for key in keys { try k.set(key, v) } }
            }, mmkv: { _ in
                timeNs { for key in keys { m.set(key, v) } }
            })
        }
    }

    func testColdSet() throws {
        for size in SIZES {
            let v = payload(size)
            let k = try KevyEngine("cset"), m = MmkvEngine("cset")
            let fresh = { (r: Int) in (0..<OPS).map { "r\(r)k\($0)" } }
            try cell("set_cold", bytes: size, ops: OPS, kevy: { r in
                let keys = fresh(r)
                return try timeNs { for key in keys { try k.set(key, v) } }
            }, mmkv: { r in
                let keys = fresh(r)
                return timeNs { for key in keys { m.set(key, v) } }
            })
        }
    }

    func testReopenedGet() throws {
        let size = 256, v = payload(size)
        let k = try KevyEngine("rget"), m = MmkvEngine("rget")
        let keys = (0..<OPS).map { "k\($0)" }
        for key in keys { try k.set(key, v); m.set(key, v) }
        try cell("get_reopened", bytes: size, ops: OPS, kevy: { _ in
            k.close(); try k.open()
            return try timeNs { for key in keys { let got = try k.get(key)?.count; precondition(got == size) } }
        }, mmkv: { _ in
            m.close(); m.open()
            return timeNs { for key in keys { precondition(m.get(key)?.count == size) } }
        })
    }

    func testBatch() throws {
        let size = 256, v = payload(size), n = 1000
        let k = try KevyEngine("batch"), m = MmkvEngine("batch")
        let fresh = { (r: Int) in (0..<n).map { "r\(r)k\($0)" } }
        try cell("batch_set", bytes: size, ops: n, kevy: { r in
            let keys = fresh(r)
            return try timeNs { try k.mset(keys, v) }
        }, mmkv: { r in
            let keys = fresh(r)
            return timeNs { for key in keys { m.set(key, v) } }
        })
        let keys = fresh(0)
        try cell("batch_get", bytes: size, ops: n, kevy: { _ in
            try timeNs { let got = try k.mget(keys)
                precondition(got.count == n && got.allSatisfy { if case .bulk(let d) = $0 { return d.count == size }; return false }) }
        }, mmkv: { _ in
            timeNs { for key in keys { precondition(m.get(key)?.count == size) } }
        })
    }

    func testStartup() throws {
        let size = 256, v = payload(size), n = 10_000
        let k = try KevyEngine("boot"), m = MmkvEngine("boot")
        for i in 0..<n { try k.set("k\(i)", v); m.set("k\(i)", v) }
        let probe = "k\(n - 1)"
        try cell("open_10k", bytes: size, ops: 1, kevy: { _ in
            k.close()
            return try timeNs { try k.open(); let got = try k.get(probe)?.count; precondition(got == size) }
        }, mmkv: { _ in
            m.close()
            return timeNs { m.open(); precondition(m.get(probe)?.count == size) }
        })
        try cell("close_10k", bytes: size, ops: 1, kevy: { _ in
            defer { try! k.open() }
            return timeNs { k.close() }
        }, mmkv: { _ in
            defer { m.open() }
            return timeNs { m.close() }
        })
    }
}
