// swift-tools-version:5.9
// KevyKit — kevy embedded for Swift (iOS / macOS), wrapping the kevy-ffi
// C ABI shipped as Kevy.xcframework.
//
// Development builds point the binary target at the in-repo build product:
//   packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
// The published package pins a release URL + checksum instead.
import PackageDescription

let package = Package(
    name: "KevyKit",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [
        .library(name: "KevyKit", targets: ["KevyKit"])
    ],
    targets: [
        .binaryTarget(name: "Kevy", path: "Artifacts/Kevy.xcframework"),
        .target(name: "KevyKit", dependencies: ["Kevy"]),
        .testTarget(name: "KevyKitTests", dependencies: ["KevyKit"]),
    ]
)
