@echo off
REM Set up the VS Build Tools MSVC environment and run the hidra Windows
REM conformance test. Invoked by the windows-test-vm-run wrapper after the hidra
REM source has been pushed to C:\hidra. %1 = SDK version (unused by cargo, kept
REM for symmetry with the build step).
setlocal

set VCVARS="C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if not exist %VCVARS% ( echo ERROR: vcvars64.bat not found at %VCVARS% & exit /b 1 )
call %VCVARS%
if errorlevel 1 exit /b 1

set PATH=%USERPROFILE%\.cargo\bin;%PATH%
REM Fail loudly instead of self-skipping if the driver/DLL is missing.
set HIDRA_WINDOWS_REQUIRED=1
set HIDRA_WINUHID_DLL=C:\WinUHid\build\Release\x64\WinUHid.dll

cd /d C:\hidra\e2e || exit /b 1
cargo test -p windows -- --nocapture
exit /b %ERRORLEVEL%
