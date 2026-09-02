# Reproducible Windows 11 test VM for the hidra Windows HID backend, built with
# wfvm (https://git.m-labs.hk/m-labs/wfvm). Mirrors the Linux `test-vm.nix`.
#
# Returns four attrs (each a COW layer on the previous), exposed as flake pkgs
# windows-test-vm{-base,-toolchain} / windows-test-vm / windows-test-vm-run:
#   baseImage      — Win11 + SSH + tweaks + test signing. No external downloads,
#                    builds from just the Windows ISO; smoke-tests the wfvm path.
#   toolchainImage — + VC++ RT, rustup MSVC, VS Build Tools, WDK, the WDK VS
#                    driver-targets extension, and the signing cert. Networked
#                    (__noChroot); the expensive, cacheable stage.
#   image          — + build/sign/install the WinUHid driver. Offline, sandboxed,
#                    fast to iterate; its winuhid-build.cmd output goes to the log.
#   run            — boots `image` in snapshot mode, pushes the current checkout
#                    and runs `cargo test -p windows` over SSH.
#
# The toolchain deliberately avoids the ~12 GB EWDK ISO: the MSVC compiler comes
# from VS Build Tools (VCTools), the driver bits from the WDK; the Windows ISO is
# the only pinned ISO. The toolchain boot is not hermetic (downloads current
# VS/WDK), so build it with `--option sandbox relaxed` (see README).
#
# Verified end-to-end: `nix run .#windows-test-vm-run` boots the image, creates a
# virtual HID device via the installed WinUHid UMDF driver, and hidra passes the
# full conformance suite (`test result: ok. 1 passed`). Build the image layers
# with `--option sandbox relaxed` (the toolchain boot needs the network).
{
  pkgs, # our flake's (unstable) nixpkgs — used only for fetches / source pinning
  wfvm, # wfvm.lib (makeWindowsImage / layers / utils), on wfvm's own nixpkgs pin
  self, # flake self, for the hidra source tree
  # WindowsTargetPlatformVersion for the WinUHid driver build + the Win11 SDK
  # component installed with VS Build Tools. Keep these in lockstep.
  sdkVersion ? "10.0.26100.0",
  # Setup/UI/input/locale language. MUST match the ISO's language or Windows
  # Setup stalls at the interactive language picker (wfvm's autounattend applies
  # this in the WinPE pass). The default ISO is English International = en-GB.
  locale ? "en-GB",
  # Windows 11 install ISO. Add it to the store first:
  #   nix-store --add-fixed sha256 Win11_25H2_EnglishInternational_x64_v2.iso
  # Override `windowsImage` to use a different ISO (update name + sha256).
  windowsImage ? pkgs.requireFile rec {
    name = "Win11_25H2_EnglishInternational_x64_v2.iso";
    sha256 = "66b7b4b71763ed6f9b2ce29326ed9284544da6f5283d00329921540c01aaaeea";
    message = ''
      Add the Windows 11 ISO to the store with:
        nix-store --add-fixed sha256 ${name}
      Download it from
        https://www.microsoft.com/software-download/windows11
      (or override `windowsImage` in e2e/platform/windows/test-vm.nix with your
      own ISO's name + sha256). Needs NIXPKGS_ALLOW_UNFREE=1 to evaluate.
    '';
  },
}:

let
  inherit (wfvm) makeWindowsImage layers utils;
  lib = pkgs.lib;

  # ---- base image: Win11 + SSH + offline tweaks (no external fetches) -----

  tweaks = layers.collapseLayers [
    layers.disable-firewall
    layers.disable-autosleep
    layers.disable-autolock
    layers.disable-scheduled-defrag
  ];

  # Enable test signing (WinUHid is a test-signed driver), then let the layer's
  # trailing `shutdown /s` reboot into it so later layers run test-signing-on.
  testSigning = {
    name = "test-signing";
    script = ''
      echo "Enabling test signing"
      win-exec "bcdedit /set testsigning on"
    '';
  };

  baseImage = makeWindowsImage {
    # 96 GB: VS Build Tools + WDK + the Rust toolchain + WinUHid build outgrow the
    # default (see README's disk-resize note; here we just size it up front).
    diskImageSize = "96G";
    inherit windowsImage;
    imageSelection = "Windows 11 Pro N";
    # Match the ISO language (English International) so Setup runs unattended.
    uiLanguage = locale;
    inputLocale = locale;
    systemLocale = locale;
    userLocale = locale;
    productKey = "MH37W-N47XK-V7XM9-C7227-GCQG9"; # GVLK, install-only (no activation)
    offlineInstallCommands = [
      tweaks
      testSigning
    ];
  };

  # ---- pinned downloads for the provisioning boot ------------------------

  # Microsoft rev's the aka.ms/vs/17/release/* bootstrapper URLs in place, so a
  # hash pinned against one breaks on the next rev. Those URLs redirect to a
  # download.visualstudio.microsoft.com permalink whose second path segment is the
  # file's own sha256 — content-addressed and immutable, so pin that instead. To
  # move to a newer bootstrapper:
  #   curl -sIL https://aka.ms/vs/17/release/<name> | grep -i ^location
  # and take the new URL + the hash from its path.
  #
  # VC++ 2015-2022 x64 runtime: a fresh Win11 lacks vcruntime140.dll, which both
  # the MSVC test binary and WinUHid.dll need (see README).
  vcRedist = pkgs.fetchurl {
    name = "vc_redist.x64.exe";
    url = "https://download.visualstudio.microsoft.com/download/pr/9d270333-8b7b-4f96-9458-6fcdb2ec0b25/CC0FF0EB1DC3F5188AE6300FAEF32BF5BEEBA4BDD6E8E445A9184072096B713B/VC_redist.x64.exe";
    sha256 = "sha256-zA/w6x3D9RiK5jAPrvMr9b7rpL3W6ORFqRhAcglrcTs=";
  };

  # rustup + VS Build Tools bootstrappers: small online installers that pull the
  # real payload during the (networked) provisioning boot.
  rustupInit = pkgs.fetchurl {
    name = "rustup-init.exe";
    url = "https://static.rust-lang.org/rustup/archive/1.29.1/x86_64-pc-windows-msvc/rustup-init.exe";
    sha256 = "sha256-b0vvZiYSYfy0MTG+hyC6uBfUA6Ce3sdFXDcZdLkL234=";
  };
  vsBuildTools = pkgs.fetchurl {
    name = "vs_BuildTools.exe";
    url = "https://download.visualstudio.microsoft.com/download/pr/fa619120-9c0e-47e6-bfe0-3ee96fb671b2/2aeac090a9cfb2c56474aa9a6c5817ad8cfb879539e0ed1aecec33de9fc2dc4f/vs_BuildTools.exe";
    sha256 = "sha256-KurAkKnPssVkdKqabFgXrYz7h5U54O0a7Owz3p/C3E8=";
  };

  # WinUHid: cgutman's userland virtual-HID framework (test-signed KMDF/VHF
  # driver + loader DLL). Built from source in-guest.
  winuhidSrc = pkgs.fetchFromGitHub {
    owner = "cgutman";
    repo = "WinUHid";
    rev = "d6cebbef5c7909168d1f881185be8f607d6aefd4";
    hash = "sha256-Em+lyIpHgOlKZtWAEqKzUlkpknVyaSxBKuv6SOx2p7s=";
  };

  # ---- provisioning: two stages so the WinUHid build iterates cheaply ----
  #
  # Both stages are hand-rolled qemu boots (modeled on wfvm's win.nix
  # finalOfflineImage) on a COW layer over the previous image.
  #
  #   toolchainImage — installs VC++ RT, rustup, VS Build Tools, WDK, the WDK VS
  #     driver-targets extension and the signing cert. Needs the internet, so it
  #     is __noChroot (build with `--option sandbox relaxed`). Expensive (~GBs of
  #     downloads) but changes rarely, so it caches and is reused.
  #   provisionedImage — builds + signs + installs WinUHid on top. Fully offline
  #     (restrict=on), so it runs sandboxed and re-runs fast while iterating the
  #     driver build. Its winuhid.ps1 output is captured into the nix log.
  #
  # Each stage references only its own script (not the whole ./provision dir), so
  # editing one doesn't invalidate the other's cached image.
  toolchainScript = ./provision/toolchain.ps1;
  winuhidScript = ./provision/winuhid.ps1;

  # Boilerplate shared by both boots: start swtpm, make a COW over `base`, boot
  # it (network per `restrict`), and kill the VM/TPM on exit (no orphans).
  bootPrelude = base: restrict: ''
    ${utils.tpmStartCommands}
    trap 'jobs -p | xargs -r kill 2>/dev/null' EXIT
    qemu-img create -F qcow2 -f qcow2 -b ${base} c.img
    set -m
    qemu-system-x86_64 ${
      lib.concatStringsSep " " (
        utils.mkQemuFlags [
          "-display none"
          "-drive"
          "file=c.img,index=0,media=disk,if=virtio,cache=unsafe"
          "-netdev"
        ]
      )
    } "user,id=n1,net=192.168.1.0/24,restrict=${restrict},hostfwd=tcp::2022-:22" &
    win-wait
  '';

  qemuBuildInputs = [
    utils.qemu
    utils.win-wait
    utils.win-exec
    utils.win-put
    utils.win-get
    pkgs.qemu # qemu-img
  ];

  toolchainImage = pkgs.stdenv.mkDerivation {
    name = "hidra-windows-toolchain.img";
    __noChroot = true; # needs the internet; build with `--option sandbox relaxed`
    requiredSystemFeatures = [ "kvm" ];
    nativeBuildInputs = qemuBuildInputs;
    buildCommand = ''
      set -x
      ${bootPrelude baseImage "off"}

      echo "Pushing toolchain installers..."
      ln -s ${rustupInit} ./rustup-init.exe && win-put rustup-init.exe /C:/
      ln -s ${vsBuildTools} ./vs_BuildTools.exe && win-put vs_BuildTools.exe /C:/
      ln -s ${vcRedist} ./vc_redist.x64.exe && win-put vc_redist.x64.exe /C:/
      cp ${toolchainScript} toolchain.ps1 && chmod u+w toolchain.ps1
      win-put toolchain.ps1 /C:/

      echo "Installing toolchain (VC++ RT, rustup, VS Build Tools, WDK, cert)..."
      win-exec 'powershell.exe -ExecutionPolicy Bypass -File C:\toolchain.ps1 -SdkVersion ${sdkVersion}'

      echo "Shutting down..."
      win-exec 'shutdown /s /t 0'
      fg || true
      mv c.img $out
    '';
  };

  provisionedImage = pkgs.stdenv.mkDerivation {
    name = "hidra-windows.img";
    # __noChroot: cargo/driver tooling may reach the network; runs on top of the
    # cached toolchain, so it stays fast. Build with `--option sandbox relaxed`.
    __noChroot = true;
    requiredSystemFeatures = [ "kvm" ];
    nativeBuildInputs = qemuBuildInputs;
    buildCommand = ''
      set -x
      ${bootPrelude toolchainImage "off"}

      echo "Pushing WinUHid source..."
      cp -r ${winuhidSrc} WinUHid && chmod -R u+w WinUHid
      win-put WinUHid /C:/
      cp ${winuhidScript} winuhid.ps1 && chmod u+w winuhid.ps1
      win-put winuhid.ps1 /C:/

      echo "Building/signing/installing WinUHid..."
      set +e
      win-exec 'cmd /c "powershell -ExecutionPolicy Bypass -File C:\winuhid.ps1 ${sdkVersion} > C:\wu.log 2>&1"'
      rc=$?
      set -e
      echo "----------------- winuhid.ps1 output -----------------"
      win-get /C:/wu.log || true
      cat wu.log || echo "(no wu.log retrieved)"
      echo "----------------- end winuhid.ps1 (rc=$rc) -----------"
      if [ "$rc" -ne 0 ]; then
        echo "WinUHid build/install failed (rc=$rc)"; exit 1
      fi

      echo "Shutting down..."
      win-exec 'shutdown /s /t 0'
      fg || true
      mv c.img $out
    '';
  };

  # ---- run wrapper: boot snapshot, push hidra, cargo test ----------------

  runScript = pkgs.writeShellScriptBin "windows-test-vm-run" ''
    set -e -m
    # Kill the VM (and swtpm) on any exit so a failed run doesn't orphan a qemu
    # holding port 2022.
    trap 'jobs -p | xargs -r kill 2>/dev/null' EXIT
    export PATH=${
      lib.makeBinPath [
        utils.qemu
        utils.win-wait
        utils.win-exec
        utils.win-put
      ]
    }:$PATH

    ${utils.tpmStartCommands}

    qemu-system-x86_64 ${
      lib.concatStringsSep " " (
        utils.mkQemuFlags [
          "-drive"
          "file=${provisionedImage},index=0,media=disk,if=virtio,cache=unsafe"
          "-snapshot"
          # restrict=off: cargo fetches crate deps from the internet during the test.
          "-netdev"
          "user,id=n1,net=192.168.1.0/24,restrict=off,hostfwd=tcp::2022-:22"
        ]
      )
    } &

    win-wait

    echo "Pushing hidra source..."
    cp -r ${self} hidra && chmod -R u+w hidra
    win-put hidra /C:/

    echo "Running windows conformance test..."
    win-exec 'cmd /c C:\hidra\e2e\platform\windows\provision\runtest.cmd ${sdkVersion}'

    echo "Shutting down..."
    win-exec 'shutdown /s /t 0'
    fg || true
    echo "Done"
  '';

in
{
  inherit baseImage toolchainImage;
  image = provisionedImage;
  run = runScript;
}
