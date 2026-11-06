import ProjectDescription

let project = Project(
    name: "QuotaBar",
    options: .options(
        defaultKnownRegions: ["en"],
        developmentRegion: "en"
    ),
    settings: .settings(
        base: [
            "SWIFT_VERSION": "6.0",
            "MACOSX_DEPLOYMENT_TARGET": "15.0",
            "ENABLE_DEBUG_DYLIB": "YES",
        ],
        debug: [
            "SWIFT_ACTIVE_COMPILATION_CONDITIONS": "DEBUG MOCKING",
        ]
    ),
    targets: [
        // MARK: - Domain Layer
        .target(
            name: "Domain",
            destinations: .macOS,
            product: .staticFramework,
            bundleId: "com.aakshintala.subscriptionusagebar.domain",
            deploymentTargets: .macOS("15.0"),
            sources: ["Sources/Domain/**"],
            dependencies: [
                .external(name: "Mockable"),
            ],
            settings: .settings(
                base: [
                    "SWIFT_STRICT_CONCURRENCY": "complete",
                ]
            )
        ),

        // MARK: - Infrastructure Layer
        .target(
            name: "Infrastructure",
            destinations: .macOS,
            product: .staticFramework,
            bundleId: "com.aakshintala.subscriptionusagebar.infrastructure",
            deploymentTargets: .macOS("15.0"),
            sources: ["Sources/Infrastructure/**"],
            dependencies: [
                .target(name: "Domain"),
                .external(name: "Mockable"),
            ],
            settings: .settings(
                base: [
                    "SWIFT_STRICT_CONCURRENCY": "complete",
                ]
            )
        ),

        // MARK: - Main Application
        .target(
            name: "QuotaBar",
            destinations: .macOS,
            product: .app,
            bundleId: "com.aakshintala.subscriptionusagebar",
            deploymentTargets: .macOS("15.0"),
            infoPlist: .file(path: "Sources/App/Info.plist"),
            sources: ["Sources/App/**"],
            resources: [
                "Sources/App/Resources/**",
            ],
            entitlements: .file(path: "Sources/App/entitlements.plist"),
            scripts: [
                .pre(
                    script: """
                    set -eu
                    export PATH="/opt/homebrew/bin:/usr/local/bin:${PATH}"
                    if ! command -v go >/dev/null 2>&1; then
                      echo "error: go not found (looked in /opt/homebrew/bin and /usr/local/bin). Install Go to build quotad." >&2
                      exit 1
                    fi
                    build_dir="${DERIVED_FILE_DIR}/quotad-build"
                    rm -rf "${build_dir}"
                    mkdir -p "${build_dir}"
                    n=0
                    for arch in ${ARCHS}; do
                      case "${arch}" in
                        arm64) goarch=arm64 ;;
                        x86_64) goarch=amd64 ;;
                        *) echo "error: unsupported ARCHS entry '${arch}' for quotad build" >&2; exit 1 ;;
                      esac
                      (cd "${SRCROOT}/quotad" && CGO_ENABLED=0 GOOS=darwin GOARCH=$goarch go build -trimpath -ldflags='-s -w' -o "${build_dir}/quotad-${goarch}" .)
                      n=$((n + 1))
                    done
                    dest="${TARGET_BUILD_DIR}/${EXECUTABLE_FOLDER_PATH}"
                    mkdir -p "${dest}" "${TARGET_BUILD_DIR}/${CONTENTS_FOLDER_PATH}/Library/LaunchAgents"
                    if [ "${n}" -gt 1 ]; then
                      lipo -create "${build_dir}"/quotad-* -output "${dest}/quotad"
                    else
                      cp "${build_dir}"/quotad-* "${dest}/quotad"
                    fi
                    chmod +x "${dest}/quotad"
                    cp "${SRCROOT}/Sources/App/LaunchAgents/com.aakshintala.quotad.plist" \
                      "${TARGET_BUILD_DIR}/${CONTENTS_FOLDER_PATH}/Library/LaunchAgents/com.aakshintala.quotad.plist"
                    identity="${EXPANDED_CODE_SIGN_IDENTITY:--}"
                    if [ "${identity}" = "-" ]; then
                      codesign --force --options runtime --sign "${identity}" "${dest}/quotad"
                    else
                      codesign --force --options runtime --timestamp --sign "${identity}" "${dest}/quotad"
                    fi
                    """,
                    name: "Build and sign bundled quotad",
                    // Runs on every build: Xcode tracks a directory input by the
                    // directory's own mtime, so edits to .go files never retrigger
                    // it. Go's build cache keeps an unchanged rebuild near a second.
                    basedOnDependencyAnalysis: false
                ),
            ],
            dependencies: [
                .target(name: "Domain"),
                .target(name: "Infrastructure"),
                .external(name: "MenuBarExtraAccess"),
            ],
            settings: .settings(
                base: [
                    "SWIFT_STRICT_CONCURRENCY": "complete",
                    "ENABLE_PREVIEWS": "YES",
                    "CODE_SIGN_IDENTITY": "-",
                    "ASSETCATALOG_COMPILER_APPICON_NAME": "AppIcon",
                ],
                debug: [
                    "SWIFT_ACTIVE_COMPILATION_CONDITIONS": "DEBUG",
                ],
                release: [
                    "CODE_SIGN_IDENTITY": "Developer ID Application",
                    "DEVELOPMENT_TEAM": "936XPH47M9",
                    "CODE_SIGN_STYLE": "Manual",
                    "ENABLE_HARDENED_RUNTIME": "YES",
                ]
            )
        ),

        // MARK: - Domain Tests
        .target(
            name: "DomainTests",
            destinations: .macOS,
            product: .unitTests,
            bundleId: "com.aakshintala.subscriptionusagebar.domain-tests",
            deploymentTargets: .macOS("15.0"),
            sources: ["Tests/DomainTests/**", "Tests/Support/**"],
            dependencies: [
                .target(name: "Domain"),
                .target(name: "Infrastructure"),
                .external(name: "Mockable"),
            ],
            settings: .settings(
                base: [
                    "SWIFT_ACTIVE_COMPILATION_CONDITIONS": "MOCKING",
                ]
            )
        ),

        // MARK: - Infrastructure Tests
        .target(
            name: "InfrastructureTests",
            destinations: .macOS,
            product: .unitTests,
            bundleId: "com.aakshintala.subscriptionusagebar.infrastructure-tests",
            deploymentTargets: .macOS("15.0"),
            sources: ["Tests/InfrastructureTests/**", "Tests/Support/**"],
            dependencies: [
                .target(name: "Infrastructure"),
                .target(name: "Domain"),
                .external(name: "Mockable"),
            ],
            settings: .settings(
                base: [
                    "SWIFT_ACTIVE_COMPILATION_CONDITIONS": "MOCKING",
                ]
            )
        ),

        // MARK: - Acceptance Tests (BDD - Outer Loop)
        .target(
            name: "AcceptanceTests",
            destinations: .macOS,
            product: .unitTests,
            bundleId: "com.aakshintala.subscriptionusagebar.acceptance-tests",
            deploymentTargets: .macOS("15.0"),
            sources: ["Tests/AcceptanceTests/**", "Tests/Support/**"],
            dependencies: [
                .target(name: "Domain"),
                .target(name: "Infrastructure"),
                .external(name: "Mockable"),
            ],
            settings: .settings(
                base: [
                    "SWIFT_ACTIVE_COMPILATION_CONDITIONS": "MOCKING",
                ]
            )
        ),
    ],
    schemes: [
        .scheme(
            name: "QuotaBar",
            shared: true,
            buildAction: .buildAction(targets: ["QuotaBar"]),
            testAction: .targets(
                [
                    .testableTarget(target: .target("AcceptanceTests")),
                    .testableTarget(target: .target("DomainTests")),
                    .testableTarget(target: .target("InfrastructureTests")),
                ],
                configuration: .debug
            ),
            runAction: .runAction(configuration: .debug, executable: .target("QuotaBar")),
            archiveAction: .archiveAction(configuration: .release),
            profileAction: .profileAction(configuration: .release, executable: .target("QuotaBar")),
            analyzeAction: .analyzeAction(configuration: .debug)
        ),
    ]
)