// swift-tools-version:5.9
// mmkvgate (Apple) — kevy's scalar fast path vs MMKV, measured on an iOS
// simulator via XCTest. The bar for the mobile door is MMKV's synchronous
// mmap get/set; this harness puts them side by side across value size and
// cold/warm, and the numbers land in bench/mmkvgate/LEDGER.md as-is —
// losing axes named, not hidden.
//
//   xcodebuild test -scheme mmkvgate \
//     -destination 'platform=iOS Simulator,name=iPhone 16'
import PackageDescription

let package = Package(
    name: "mmkvgate",
    platforms: [.iOS(.v15)],
    dependencies: [
        .package(path: "../../../bindings/apple/KevyKit"),
        .package(url: "https://github.com/Tencent/MMKV.git", from: "2.4.0"),
    ],
    targets: [
        .testTarget(
            name: "mmkvgateTests",
            dependencies: [
                .product(name: "KevyKit", package: "KevyKit"),
                .product(name: "MMKV", package: "MMKV"),
            ]
        )
    ]
)
