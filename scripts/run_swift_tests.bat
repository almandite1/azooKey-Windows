@echo off
REM Run the Swift package tests directly, bypassing `swift test`'s broken
REM symlink step on Windows. Rebuilds via `swift build --build-tests` first.
REM
REM This is the LOCAL fallback, not the canonical runner: CI and
REM `cargo make test_swift` both use a plain `swift test` and pass. Which of
REM the two is right has not been settled (see the refactoring plan's "I"),
REM so both are kept. Prefer `cargo make test_swift`; reach for this one when
REM `swift test` fails on the symlink step on your machine.
REM
REM Lives in scripts/, so every path below climbs one level out of %~dp0.
set "ROOT=%~dp0.."
call "%ROOT%\devenv.bat"
REM `swift test` puts the swift-testing / XCTest platform libraries on PATH
REM automatically; running the .xctest exe directly does not, so add them.
set "TESTLIB=%LOCALAPPDATA%\Programs\Swift\Platforms\6.3.3\Windows.platform\Developer\Library"
set "PATH=%ROOT%\llama_cpu;%TESTLIB%\Testing-6.3.3\usr\bin64;%TESTLIB%\XCTest-6.3.3\usr\bin64;%PATH%"
pushd "%ROOT%\server-swift"
swift build --build-tests %*
if errorlevel 1 ( popd & exit /b 1 )
".build\x86_64-unknown-windows-msvc\debug\azookey-serverPackageTests.xctest" --testing-library swift-testing
set RC=%errorlevel%
popd
exit /b %RC%
