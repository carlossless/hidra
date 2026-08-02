# In-guest WinUHid stage, run on top of the cached toolchain image. Self-contained
# PowerShell: imports the VS Build Tools x64 environment, then builds, signs and
# installs cgutman's WinUHid UMDF virtual-HID driver + loader. Fast + re-runnable,
# so the driver build iterates without re-installing the toolchain.
#
# Everything runs as external commands from PowerShell (no nested `.cmd`), so all
# output is captured by the caller's `> C:\wu.log 2>&1` — invoking a `.cmd` that
# `call`s vcvars through the wfvm SSH helper drops the session (rc=255).
#
# No WDK-vsix registration: testing whether the WDK MSI alone gives Build Tools
# the driver toolset. If msbuild reports the WindowsUserModeDriver10.0 toolset
# missing, the vsix/registration step comes back.
param([string]$SdkVersion = "10.0.26100.0")
$ErrorActionPreference = "Continue"
Write-Host "== WinUHid build starting (SDK $SdkVersion) =="

$BuildTools = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
$ToolsetDir = "$BuildTools\MSBuild\Microsoft\VC\v170\Platforms\x64\PlatformToolsets\WindowsUserModeDriver10.0"

# The WDK MSI doesn't register the UMDF/KMDF MSBuild toolsets into Build Tools
# (that MSI integration is VS2026+). The headless-reliable fix is to extract the
# WDK.vsix (a zip) and copy its $MSBuild payload into Build Tools; VSIXInstaller
# hangs/fails on Build Tools. Skip if the toolset is already present.
if (-not (Test-Path $ToolsetDir)) {
    Write-Host "== Registering WDK MSBuild toolset into Build Tools =="
    $vsix = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\Vsix" -Recurse -Filter "WDK.vsix" -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\(amd64|x64)\\' } | Select-Object -First 1
    if (-not $vsix) { Write-Host "ERROR: WDK.vsix (amd64/x64) not found"; exit 3 }
    Write-Host "Extracting $($vsix.FullName)"
    $ex = "C:\wdkvsix"
    Remove-Item $ex -Recurse -Force -ErrorAction SilentlyContinue
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory($vsix.FullName, $ex)
    Write-Host "vsix top-level entries:"
    Get-ChildItem $ex | ForEach-Object { Write-Host "  $($_.Name)" }
    # '$MSBuild' in the vsix maps to <VS>\MSBuild.
    $msb = Join-Path $ex '$MSBuild'
    if (Test-Path $msb) {
        Copy-Item "$msb\*" "$BuildTools\MSBuild\" -Recurse -Force
        Write-Host "Copied `$MSBuild -> BuildTools\MSBuild"
    } else {
        Write-Host "WARN: no `$MSBuild folder in vsix; dumping tree 2 levels deep:"
        Get-ChildItem $ex -Recurse -Depth 1 -Directory | ForEach-Object { Write-Host "  $($_.FullName)" }
    }
    if (Test-Path $ToolsetDir) { Write-Host "Toolset registered OK: $ToolsetDir" }
    else { Write-Host "ERROR: toolset still missing at $ToolsetDir"; exit 4 }
}

# Import the VS Build Tools x64 env (run vcvars64.bat, then capture `set`).
$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) { Write-Host "ERROR: vcvars64 not found at $vcvars"; exit 2 }
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "Env:\$($matches[1])" -Value $matches[2] }
}
Write-Host ("cl.exe:  " + (Get-Command cl.exe -ErrorAction SilentlyContinue).Source)
Write-Host ("msbuild: " + (Get-Command msbuild.exe -ErrorAction SilentlyContinue).Source)

# Add the WDK/SDK bin (inf2cat, devcon, signtool) to PATH.
$kbin = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Directory -Filter "10.*" -ErrorAction SilentlyContinue |
        Sort-Object Name | Select-Object -Last 1
if ($kbin) { $env:PATH = "$($kbin.FullName)\x64;$($kbin.FullName)\x86;$env:PATH" }

Set-Location C:\WinUHid

# msbuild's console output gets swallowed through the SSH/sftp capture chain, so
# use its file logger and dump the (clean) file afterwards.
function Invoke-MSBuild([string]$proj) {
    $log = "C:\msbuild.log"
    Remove-Item $log -ErrorAction SilentlyContinue
    # SolutionDir: building a .vcxproj directly defaults $(SolutionDir) to the
    #   project dir, scattering outputs; pin it to the repo root so both land in
    #   C:\WinUHid\build\Release\x64 (per WinUHidCppProps.props).
    # SpectreMitigation=false: driver defaults to Spectre-mitigated libs (MSB8040)
    #   which base VCTools lacks; irrelevant for a virtual test driver.
    # SignMode=Off: skip the project's own WDKTestCert signing; we sign with our
    #   pre-trusted cert below.
    & msbuild $proj /t:Build /nologo `
        /p:Configuration=Release /p:Platform=x64 /p:WindowsTargetPlatformVersion=$SdkVersion `
        "/p:SolutionDir=C:\WinUHid\\" /p:SpectreMitigation=false /p:SignMode=Off `
        /clp:Verbosity=minimal "/flp:logfile=$log;Verbosity=normal" | Out-Null
    $rc = $LASTEXITCODE
    if (Test-Path $log) {
        Write-Host "----- msbuild.log [$proj] -----"
        Get-Content $log | ForEach-Object { Write-Host $_ }
        Write-Host "----- end msbuild.log (exit $rc) -----"
    }
    return $rc
}

Write-Host "== msbuild: WinUHid Driver =="
if ((Invoke-MSBuild "WinUHid Driver\WinUHid Driver.vcxproj") -ne 0) { exit 10 }

Write-Host "== msbuild: WinUHid (loader) =="
if ((Invoke-MSBuild "WinUHid\WinUHid.vcxproj") -ne 0) { exit 11 }

$out = "C:\WinUHid\build\Release\x64"
Write-Host "== build output tree ($out) =="
Get-ChildItem $out -Recurse -File -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $($_.FullName)" }

# Loader DLL the test loads (HIDRA_WINUHID_DLL points here).
$loader = "$out\WinUHid.dll"
if (-not (Test-Path $loader)) { Write-Host "ERROR: loader not at $loader"; exit 12 }

# UMDF driver projects emit a package subfolder; locate the INF wherever it landed.
$inf = Get-ChildItem $out -Recurse -Filter "WinUHidDriver.inf" -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $inf) { Write-Host "ERROR: WinUHidDriver.inf not found under $out"; exit 13 }
$pkg = $inf.DirectoryName
Write-Host "driver package: $pkg"

# Regenerate the catalog so it hashes the built (unsigned) driver files, then
# sign ONLY the .cat. Do NOT embed-sign the .dll afterwards — that changes the
# dll and breaks the catalog's hash, so Windows silently rejects the package
# (the earlier "devcon returns 0 but driver never lands in the store" bug).
Write-Host "== catalog + sign (.cat only, trusted 'WFVM WinUHid Test' cert) =="
Remove-Item "$pkg\*.cat" -Force -ErrorAction SilentlyContinue
& inf2cat /driver:"$pkg" /os:10_x64
if ($LASTEXITCODE -ne 0) { Write-Host "inf2cat failed ($LASTEXITCODE)"; exit 13 }
$cat = (Get-ChildItem "$pkg\*.cat" | Select-Object -First 1).FullName
& signtool sign /fd sha256 /sm /n "WFVM WinUHid Test" $cat
if ($LASTEXITCODE -ne 0) { Write-Host "signtool failed ($LASTEXITCODE)"; exit 13 }

Write-Host "== package contents =="
Get-ChildItem $pkg | ForEach-Object { Write-Host "  $($_.Name)" }

# Publish to the DriverStore, then create the Root\WinUHid enumerator device.
Write-Host "== pnputil /add-driver /install =="
& pnputil /add-driver $inf.FullName /install 2>&1 | ForEach-Object { Write-Host $_ }

# devcon lives under the WDK's Tools\<ver>\<arch>, NOT bin. (Resolving it wrong
# left `& devcon` unfound while $LASTEXITCODE stayed 0 from the prior command, so
# the device was never created yet the build "succeeded".)
$devcon = (Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\Tools" -Recurse -Filter devcon.exe -ErrorAction SilentlyContinue |
           Where-Object { $_.FullName -match "\\x64\\" } | Select-Object -First 1).FullName
if (-not $devcon) { Write-Host "ERROR: devcon.exe not found under WDK Tools"; exit 15 }
Write-Host "== devcon install (Root\WinUHid) via $devcon =="
& $devcon install $inf.FullName "Root\WinUHid" 2>&1 | ForEach-Object { Write-Host $_ }
if ($LASTEXITCODE -ne 0) { Write-Host "devcon install failed ($LASTEXITCODE)"; exit 15 }

# Success criterion: the enumerator device exists and started cleanly.
Write-Host "== verify enumerator device present + started =="
$dev = Get-PnpDevice -Class System -ErrorAction SilentlyContinue |
       Where-Object { $_.FriendlyName -like "*WinUHid*" } | Select-Object -First 1
if (-not $dev) { Write-Host "ERROR: WinUHid enumerator device not created"; exit 16 }
Write-Host ("device: {0} [{1}] Status={2} Problem={3}" -f $dev.FriendlyName, $dev.InstanceId, $dev.Status, $dev.Problem)
if ($dev.Problem -ne "CM_PROB_NONE") { Write-Host "ERROR: device problem $($dev.Problem)"; exit 17 }

Write-Host "== WinUHid built + installed (loader: $loader) =="
exit 0
