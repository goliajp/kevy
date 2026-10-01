// Timing and reporting shared by every axis.
//
// Both engines run inside the same test in alternating rounds, so on a
// phone they share one thermal state; each engine's figure is the median
// of its rounds, after one discarded warm-up round.
import Foundation
import XCTest

let ROUNDS = 7

func payload(_ n: Int) -> Data { Data(repeating: 0x61, count: n) }

func tmpDir(_ tag: String) -> String {
    let d = NSTemporaryDirectory() + "mmkvgate-\(tag)-\(UUID().uuidString)"
    try! FileManager.default.createDirectory(atPath: d, withIntermediateDirectories: true)
    return d
}

@inline(never)
func timeNs(_ body: () throws -> Void) rethrows -> UInt64 {
    let t0 = DispatchTime.now().uptimeNanoseconds
    try body()
    return DispatchTime.now().uptimeNanoseconds - t0
}

func median(_ xs: [UInt64]) -> UInt64 { xs.sorted()[xs.count / 2] }

/// One cell of the table. The closures get the round index (-1 for the
/// warm-up, so a cold axis can use fresh key names each round) and return
/// nanoseconds for `ops` operations; `ops` is 1 for axes measured per
/// event, like opening a store.
func cell(_ axis: String, bytes: Int, ops: Int,
          kevy: (Int) throws -> UInt64, mmkv: (Int) throws -> UInt64) rethrows {
    _ = try kevy(-1)
    _ = try mmkv(-1)
    var k: [UInt64] = []
    var m: [UInt64] = []
    for r in 0..<ROUNDS {
        k.append(try kevy(r))
        m.append(try mmkv(r))
    }
    let kn = Double(median(k)) / Double(ops)
    let mn = Double(median(m)) / Double(ops)
    print(String(format: "MMKVGATE %@ %d kevy_ns=%.1f mmkv_ns=%.1f kevy/mmkv=%.3f",
                 axis, bytes, kn, mn, kn / mn))
}
