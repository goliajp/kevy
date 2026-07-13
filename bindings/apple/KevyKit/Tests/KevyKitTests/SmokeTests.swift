// The Swift smoke — the same contract as the C/Go/Bun ones, plus the
// typed-throws opinion and the scalar fast path.
import XCTest

@testable import KevyKit

final class SmokeTests: XCTestCase {
    func testSmoke() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("kevy-swift-\(UUID().uuidString)/data").path

        var db = try KevyDB(dir: dir)

        // scalar fast path
        try db.set("k", Data("v".utf8))
        XCTAssertEqual(try db.getText("k"), "v")
        XCTAssertNil(try db.get("absent"))

        // TTL through the fast path, observed through the verb surface
        try db.set("tmp", Data("x".utf8), ttlMs: 30_000)
        let ttl = try db.pttl("tmp")
        XCTAssertTrue(ttl > 0 && ttl <= 30_000, "pttl = \(ttl)")

        // typed layer: WRONGTYPE throws
        XCTAssertEqual(try db.incr("hits"), 1)
        XCTAssertThrowsError(try db.incr("k"))

        // cmd stays neutral: a protocol error is a value
        let bad = try db.cmd(["NOSUCHVERB"])
        XCTAssertTrue(bad.isError)

        // pub/sub round trip
        let sub = try db.subscribe("c1")
        XCTAssertNotNil(try sub.next()) // subscribe ack
        XCTAssertEqual(try db.publish("c1", Data("hello".utf8)), 1)
        guard case .array(let frame)? = try sub.next() else {
            return XCTFail("missing message frame")
        }
        XCTAssertEqual(frame[0].text, "message")
        XCTAssertEqual(frame[1].text, "c1")
        XCTAssertEqual(frame[2].text, "hello")
        XCTAssertNil(try sub.next()) // drained
        sub.close()

        XCTAssertEqual(try db.keys().sorted(), ["hits", "k", "tmp"])
        XCTAssertEqual(try db.dbsize(), 3)

        // durability: close, reopen, the keys survived
        db.close()
        db = try KevyDB(dir: dir)
        XCTAssertEqual(try db.getText("k"), "v")
        XCTAssertEqual(try db.del("k", "absent"), 1)
        try db.flushall()
        XCTAssertEqual(try db.dbsize(), 0)
        db.close()
    }
}
