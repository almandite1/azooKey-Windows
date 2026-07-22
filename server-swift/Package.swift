// swift-tools-version: 6.1
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

// The converter enables C++ interoperability whenever the Zenzai trait is on
// (it talks to llama.cpp), and Swift requires every client of such a module
// to compile with it too. Nothing here uses C++ directly; this only makes
// `import KanaKanjiConverterModule` legal.
let swiftSettings: [SwiftSetting] = [
    .interoperabilityMode(.Cxx)
]

let package = Package(
    name: "azookey-server",
    products: [
        // Products define the executables and libraries a package produces, making them visible to other packages.
        .library(
            name: "azookey-server",
            type: .dynamic,
            targets: ["azookey-server"]
        ),
        .library(name: "ffi", targets: ["azookey-server"])
    ],
    dependencies: [
        // Dependencies declare other packages that this package depends on.
        // .package(url: /* package url */, from: "1.0.0"),
        // Zenzai is opt-in upstream since the package adopted SwiftPM traits;
        // without it the converter compiles against llama-mock.swift and
        // neural conversion silently disappears. "Zenzai" (not "ZenzaiCPU")
        // because ZenzaiCPU pins n_gpu_layers to 0, which would rule out the
        // GPU offload issue #56 asks for.
        .package(
            url: "https://github.com/azookey/AzooKeyKanaKanjiConverter",
            revision: "bbef9d2d99a2e9e69ac3f7e2e07b08474de59a81",
            traits: ["Zenzai"]
        )
    ],
    targets: [
        // Targets are the basic building blocks of a package, defining a module or a test suite.
        // Targets can depend on other targets in this package and products from dependencies.
        .target(name: "ffi"),
        .target(
            name: "azookey-server",
            dependencies: [
                .product(name: "KanaKanjiConverterModule", package: "azookeykanakanjiconverter"),
                "ffi"
            ],
            swiftSettings: swiftSettings
        ),
        .testTarget(
            name: "azookey-serverTests",
            dependencies: ["azookey-server"],
            swiftSettings: swiftSettings
        ),
    ]
)
