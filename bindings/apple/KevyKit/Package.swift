// swift-tools-version:5.9
// The KevyKit package manifest lives at the REPOSITORY ROOT, not here.
//
// SwiftPM resolves a package from the root of the repository it clones,
// so a manifest in this directory could never be reached by
// `.package(url: "https://github.com/goliajp/kevy", …)` — the line this
// directory's README has always shown. Two manifests would also drift:
// this one built its binary target from "Artifacts/…", the root one from
// "bindings/apple/KevyKit/Artifacts/…", and only one of them is the one
// consumers get.
//
// Sources, tests and Artifacts all stay here. Edit ../../../Package.swift.
import PackageDescription

let package = Package(
    name: "KevyKit-moved",
    products: [],
    targets: []
)
