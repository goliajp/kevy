// swift-tools-version:5.9
// KevyKit — kevy embedded for Swift (iOS / macOS), wrapping the kevy-ffi
// C ABI shipped as Kevy.xcframework.
//
// This manifest lives at the REPOSITORY ROOT because SwiftPM resolves a
// package from the root of the repository it clones — there is no
// subdirectory-package form the way Go modules have one. The sources
// stay where they are under bindings/apple/KevyKit; only the manifest
// moves, so `.package(url: "https://github.com/goliajp/kevy", from:
// "5.1.0")` — the line KevyKit's README has always shown — resolves
// without a second repository to create, mirror and keep in step.
//
// Development builds point the binary target at the in-repo build
// product:
//   packaging/apple/build-xcframework.sh bindings/apple/KevyKit/Artifacts
// A published release pins a release URL + checksum instead.
import PackageDescription

let package = Package(
    name: "KevyKit",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [
        .library(name: "KevyKit", targets: ["KevyKit"])
    ],
    targets: [
        .binaryTarget(
            name: "Kevy",
            path: "bindings/apple/KevyKit/Artifacts/Kevy.xcframework"
        ),
        .target(
            name: "KevyKit",
            dependencies: ["Kevy"],
            path: "bindings/apple/KevyKit/Sources/KevyKit"
        ),
        .testTarget(
            name: "KevyKitTests",
            dependencies: ["KevyKit"],
            path: "bindings/apple/KevyKit/Tests/KevyKitTests"
        ),
    ]
)
