# NixOS VM running hidra's full Linux test matrix (hidraw/uhid, nusb/dummy_hcd+g_hid,
# WebHID/Chromium). Interactive: `nix run .#test-vm`, log in as root, run-hidra-tests.
# Headless CI (auto-run + poweroff, results on the serial console):
#   QEMU_KERNEL_PARAMS="console=ttyS0 hidra.autorun" nix run .#test-vm -- -nographic
{
  pkgs,
  lib,
  self,
  ...
}:
let
  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    targets = [ "wasm32-unknown-unknown" ];
  };

  # wasm-bindgen CLI must match the wasm-bindgen crate version hidra pulls in.
  wasmBindgenVersion = "0.2.126";

  runHidraTests = pkgs.writeShellApplication {
    name = "run-hidra-tests";
    runtimeInputs = [
      rustToolchain
      pkgs.gcc
      pkgs.pkg-config
      pkgs.nodejs_22
      pkgs.wasm-pack
      pkgs.chromium
      pkgs.xvfb-run
      pkgs.git
      pkgs.gnused
      pkgs.gnugrep
      pkgs.coreutils
      pkgs.bash # e2e/targets/webhid/run.sh has a /usr/bin/env bash shebang
    ];
    text = ''
      set -euo pipefail
      export HOME=/root
      export PKG_CONFIG_PATH=${pkgs.udev.dev}/lib/pkgconfig
      export CFLAGS="-I${pkgs.udev.dev}/include"
      export LD_LIBRARY_PATH=${lib.makeLibraryPath [ pkgs.udev ]}
      export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
      export CHROMIUM=${pkgs.chromium}/bin/chromium
      export NODE=${pkgs.nodejs_22}/bin/node
      export XVFB_RUN=${pkgs.xvfb-run}/bin/xvfb-run

      # Writable copy of the immutable flake source.
      rm -rf /root/hidra
      cp -r ${self} /root/hidra
      chmod -R u+w /root/hidra

      # uhid and nusb must build in SEPARATE cargo invocations: hidra's `nusb`
      # feature switches the Linux backend, so building them together would unify it.
      cd /root/hidra/e2e

      echo "==================== uhid (hidraw backend) ===================="
      HIDRA_HIDRAW_REQUIRED=1 cargo test -p linux-hidraw -- --test-threads=1 --nocapture

      echo "==================== nusb (dummy_hcd + g_hid) ===================="
      HIDRA_NUSB_REQUIRED=1 cargo test -p linux-nusb -- --test-threads=1 --nocapture

      echo "==================== WebHID (Chromium + Playwright) ===================="
      if ! command -v wasm-bindgen >/dev/null || [ "$(wasm-bindgen --version)" != "wasm-bindgen ${wasmBindgenVersion}" ]; then
        cargo install wasm-bindgen-cli --version ${wasmBindgenVersion} --root /root/.wbg --locked
      fi
      export PATH=/root/.wbg/bin:$PATH

      cd /root/hidra/e2e/targets/webhid
      wasm-pack build --target web --dev
      PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 ${pkgs.nodejs_22}/bin/npm install
      ./run.sh   # builds the fixture crate itself

      echo "==================== ALL HIDRA LINUX TESTS PASSED ===================="
    '';
  };
in
{
  system.stateVersion = "24.11";

  boot.kernelModules = [
    "uhid"
    "dummy_hcd"
    "libcomposite"
    "usb_f_hid"
    "configfs"
  ];

  virtualisation = {
    memorySize = 6144;
    cores = 4;
    diskSize = 12288;
    graphics = false;
  };

  services.getty.autologinUser = "root";

  networking.firewall.enable = false;

  environment.systemPackages = [
    runHidraTests
    rustToolchain
    pkgs.gcc
    pkgs.pkg-config
    pkgs.nodejs_22
    pkgs.wasm-pack
    pkgs.chromium
    pkgs.xvfb-run
    pkgs.git
    pkgs.cacert
  ];

  systemd.services.hidra-autorun = {
    description = "Run hidra Linux tests then power off";
    after = [ "multi-user.target" "network-online.target" ];
    wants = [ "network-online.target" ];
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      Type = "oneshot";
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
    script = ''
      if ${pkgs.gnugrep}/bin/grep -q hidra.autorun /proc/cmdline; then
        if ${runHidraTests}/bin/run-hidra-tests; then
          echo "HIDRA_AUTORUN_RESULT=PASS"
        else
          echo "HIDRA_AUTORUN_RESULT=FAIL rc=$?"
        fi
        ${pkgs.systemd}/bin/systemctl poweroff
      fi
    '';
  };
}
