@echo off
REM Developer environment for azooKey-Windows builds.
REM vcvars must run before PATH is extended, or cmd expands %PATH% too early.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
set "SDKROOT=%LOCALAPPDATA%\Programs\Swift\Platforms\6.3.3\Windows.platform\Developer\SDKs\Windows.sdk\"
REM Inno Setup 7 (x64) comes first on PATH. 6.x may still be installed
REM alongside — a deliberate rollback path — so the order is what decides
REM which iscc `cargo make build` gets.
set "PATH=%LOCALAPPDATA%\Programs\Swift\Toolchains\6.3.3+Asserts\usr\bin;%LOCALAPPDATA%\Programs\Swift\Runtimes\6.3.3\usr\bin;%USERPROFILE%\.cargo\bin;%LOCALAPPDATA%\Microsoft\WinGet\Packages\Google.Protobuf_Microsoft.Winget.Source_8wekyb3d8bbwe\bin;%LOCALAPPDATA%\Programs\Inno Setup 7;%PATH%"
