# Builds and ad-hoc-signs the e2e/macos virtual-HID test binary with the
# restricted com.apple.developer.hid.virtual.device entitlement. Running it needs
# a real macOS kernel booted SIP-off + amfi_get_out_of_my_way (see
# ../../targets/macos/tests/macos.rs) — an unrestricted host, never a hosted CI runner.
{
  pkgs,
  self,
  version,
}:
pkgs.rustPlatform.buildRustPackage {
  pname = "hidra-macos-virtual-signed";
  inherit version;

  src = self;
  cargoRoot = "e2e";
  cargoLock.lockFile = ../../Cargo.lock;

  # sigtool provides the ad-hoc `codesign` with --entitlements support.
  nativeBuildInputs = [ pkgs.darwin.sigtool ];

  # Running the binary needs a virtual HID device + AMFI-off kernel; only build it.
  doCheck = false;

  buildPhase = ''
    runHook preBuild
    cargo test --release -p macos --no-run --offline
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    bin=$(find e2e/target -type f -perm -111 \
      -regex '.*/release/deps/macos-[0-9a-f]+' ! -name '*.d' | head -1)
    [ -n "$bin" ] || { echo "macos test binary not found"; exit 1; }
    cp "$bin" "$out/bin/macos_virtual"
    runHook postInstall
  '';

  # Sign in postFixup: fixupPhase strips the binary and re-signs it plain ad-hoc,
  # so the HID entitlement must be embedded last.
  dontStrip = true;
  postFixup = ''
    codesign -s - \
      --entitlements ${./hid-virtual-device.entitlements} \
      --force "$out/bin/macos_virtual"
  '';

  meta = {
    description = "Ad-hoc-signed hidra macOS virtual-HID integration test";
    homepage = "https://github.com/carlossless/hidra";
    license = pkgs.lib.licenses.mit;
  };
}
