# In-guest toolchain install, run once during the (networked) toolchain boot.
# Installs the VC++ runtime, Rust MSVC toolchain, VS Build Tools (VCTools + Win11
# SDK), the WDK, and a trusted self-signed code-signing cert. Registering the WDK
# VS extension and building WinUHid happen in the separate, fast winuhid stage
# (winuhid.ps1) so they iterate without re-downloading any of this.
param([string]$SdkVersion = "10.0.26100.0")
$ErrorActionPreference = "Stop"
$SdkBuild = ($SdkVersion -split '\.')[2]  # 10.0.26100.0 -> 26100

Write-Host "== Installing VC++ runtime =="
& C:\vc_redist.x64.exe /install /quiet /norestart | Out-Null

Write-Host "== Installing Rust (MSVC) toolchain via rustup =="
& C:\rustup-init.exe -y --default-host x86_64-pc-windows-msvc `
    --default-toolchain stable --profile minimal
if ($LASTEXITCODE -ne 0) { throw "rustup-init failed ($LASTEXITCODE)" }

Write-Host "== Installing VS Build Tools (VCTools + Win11 SDK $SdkBuild) =="
$vs = Start-Process -FilePath C:\vs_BuildTools.exe -Wait -PassThru -ArgumentList @(
    "--quiet", "--wait", "--norestart", "--nocache",
    "--add", "Microsoft.VisualStudio.Workload.VCTools", "--includeRecommended",
    "--add", "Microsoft.VisualStudio.Component.Windows11SDK.$SdkBuild"
)
if ($vs.ExitCode -notin 0, 3010) { throw "VS Build Tools install failed ($($vs.ExitCode))" }

Write-Host "== Installing the WDK =="
# wdksetup installs the WDK proper, but NOT its VS driver-targets extension into
# Build Tools (it targets full VS). Confirm the fwlink matches the target build.
$wdkUrl = "https://go.microsoft.com/fwlink/?linkid=2286137"  # WDK for Win11 24H2 (26100)
Invoke-WebRequest -UseBasicParsing $wdkUrl -OutFile C:\wdksetup.exe
$wdk = Start-Process -FilePath C:\wdksetup.exe -Wait -PassThru -ArgumentList @("/quiet", "/norestart")
if ($wdk.ExitCode -notin 0, 3010) { throw "WDK install failed ($($wdk.ExitCode))" }

Write-Host "== Creating + trusting self-signed code-signing certificate =="
$cert = New-SelfSignedCertificate -Type CodeSigningCert `
    -Subject "CN=WFVM WinUHid Test" `
    -CertStoreLocation Cert:\LocalMachine\My `
    -KeyUsage DigitalSignature -KeyExportPolicy Exportable `
    -NotAfter (Get-Date).AddYears(10)
Export-Certificate -Cert $cert -FilePath C:\winuhid.cer | Out-Null
# Root: validates the driver's signing chain. TrustedPublisher: suppresses the
# driver-install trust prompt (unattended). The private key stays in My (signtool
# signs from there). certutil rather than Import-Certificate: the latter is denied
# on LocalMachine\TrustedPublisher (E_ACCESSDENIED) even elevated.
& certutil -f -addstore Root C:\winuhid.cer | Out-Null
if ($LASTEXITCODE -ne 0) { throw "certutil -addstore Root failed ($LASTEXITCODE)" }
& certutil -f -addstore TrustedPublisher C:\winuhid.cer | Out-Null
if ($LASTEXITCODE -ne 0) { throw "certutil -addstore TrustedPublisher failed ($LASTEXITCODE)" }

Write-Host "== Toolchain install complete =="
