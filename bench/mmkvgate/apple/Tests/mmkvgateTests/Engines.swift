// The two engines behind one small surface, so every axis reads the same
// for both. kevy opens a durable store (AOF, fsync everysec — the
// default); MMKV is its default mmap store. Neither is the in-memory mode.
import Foundation
import KevyKit
import MMKV

private let mmkvRoot: String = {
    let root = tmpDir("mmkv-root")
    MMKV.initialize(rootDir: root, logLevel: .none)
    return root
}()

final class KevyEngine {
    let dir: String
    private(set) var db: KevyDB

    init(_ tag: String) throws {
        dir = tmpDir("kevy-\(tag)")
        db = try KevyDB(dir: dir)
    }

    func set(_ k: String, _ v: Data) throws { try db.set(k, v) }
    func get(_ k: String) throws -> Data? { try db.get(k) }

    func mset(_ keys: [String], _ v: Data) throws {
        var argv: [Data] = [Data("MSET".utf8)]
        argv.reserveCapacity(1 + keys.count * 2)
        for k in keys { argv.append(Data(k.utf8)); argv.append(v) }
        let r = try db.cmdData(argv)
        precondition(r == .simple("OK"), "MSET: \(r)")
    }

    func mget(_ keys: [String]) throws -> [KevyValue] {
        let r = try db.cmdData([Data("MGET".utf8)] + keys.map { Data($0.utf8) })
        guard case .array(let xs) = r else { preconditionFailure("MGET: \(r)") }
        return xs
    }

    func close() { db.close() }
    func open() throws { db = try KevyDB(dir: dir) }
}

final class MmkvEngine {
    let id: String
    private(set) var m: MMKV

    init(_ tag: String) {
        _ = mmkvRoot
        id = "\(tag)-\(UUID().uuidString)"
        m = MMKV(mmapID: id)!
    }

    func set(_ k: String, _ v: Data) { precondition(m.set(v, forKey: k)) }
    func get(_ k: String) -> Data? { m.data(forKey: k) }

    func close() { m.close() }
    func open() { m = MMKV(mmapID: id)! }
}
