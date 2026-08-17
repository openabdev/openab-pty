// swift-tools-version:5.9
import PackageDescription

// Prototype client for openab-pty. Implementation spec:
// openab/crates/openab-pty/CLIENT-CONTRACT.md
//
// SPM executable rather than an Xcode project because the build host has neither
// xcodegen nor brew, and a prototype does not need a bundle. It grows into a
// proper app target later without changing any of the code below.
let package = Package(
    name: "OabPtyMac",
    platforms: [.macOS(.v13)],
    dependencies: [
        // A real VT emulator. Writing one is not the point of this prototype,
        // and a text view that cannot handle ANSI would make the terminal
        // useless for the thing we actually want to test.
        .package(url: "https://github.com/migueldeicaza/SwiftTerm.git", from: "1.9.0")
    ],
    targets: [
        .executableTarget(
            name: "OabPtyMac",
            dependencies: ["SwiftTerm"],
            // tools-version 5.9 already implies the Swift 5 language mode, so
            // strict concurrency checking stays off for this prototype.
            path: "Sources/OabPtyMac"
        )
    ]
)
