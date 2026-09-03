{
  description = "hidra: pure-Rust HID library with native, nusb and WebHID backends";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Reproducible Windows 11 VM builder (M-Labs). Pins its own nixpkgs (23.11);
    # we deliberately do NOT `follows` it, makeWindowsImage is only tested against
    # that pin, and it always evaluates its layers with its own pkgs regardless.
    wfvm = {
      url = "git+https://git.m-labs.hk/m-labs/wfvm";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      wfvm,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      cargoToml = nixpkgs.lib.importTOML ./Cargo.toml;

      # e2e/targets/macos virtual-HID test binary, built + ad-hoc-signed with the HID
      # entitlement (defined in e2e/platform/macos; only runnable on a SIP-off host).
      macosVirtualSigned =
        pkgs:
        import ./e2e/platform/macos/macos-virtual-signed.nix {
          inherit pkgs self;
          version = cargoToml.package.version;
        };

      # Reproducible Windows 11 test VM (x86_64-linux + KVM only). See
      # e2e/platform/windows/test-vm.nix. Returns { image; run; }.
      windowsTestVm =
        pkgs:
        import ./e2e/platform/windows/test-vm.nix {
          inherit pkgs self;
          wfvm = wfvm.lib;
        };
    in
    {
      packages = forAllSystems (
        pkgs:
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = cargoToml.package.name;
            version = cargoToml.package.version;

            src = self;
            cargoLock.lockFile = ./Cargo.lock;

            buildFeatures = [ "nusb" ];
            cargoBuildFlags = [ "--examples" ];

            # nusb enumerates via libudev on Linux; pkg-config + udev supply the link flags.
            nativeBuildInputs = nixpkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
            buildInputs = nixpkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.udev ];

            # The crate is a library; install the example binaries for a runnable output.
            postInstall = ''
              mkdir -p $out/bin
              find target -type f -executable \
                -regex '.*/release/examples/[a-z_]+' \
                -exec install -m755 -t $out/bin {} \;
            '';

            meta = {
              description = "Pure-Rust HID library";
              homepage = "https://github.com/carlossless/hidra";
              license = nixpkgs.lib.licenses.mit;
            };
          };
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.isDarwin {
          macos-virtual-signed = macosVirtualSigned pkgs;
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          # Bootable NixOS VM for the full Linux test matrix. See e2e/platform/linux/test-vm.nix.
          test-vm = self.nixosConfigurations.hidra-test.config.system.build.vm;

          # Reproducible Windows 11 test VM. `-base` is Win11 + SSH + test signing
          # (builds from just the Windows ISO); `windows-test-vm` adds the toolchain
          # + a built/installed WinUHid driver; `-run` boots it in snapshot mode and
          # runs `cargo test -p windows` over SSH. See e2e/platform/windows/README.md.
          windows-test-vm-base = (windowsTestVm pkgs).baseImage;
          windows-test-vm-toolchain = (windowsTestVm pkgs).toolchainImage;
          windows-test-vm = (windowsTestVm pkgs).image;
          windows-test-vm-run = (windowsTestVm pkgs).run;
        }
      );

      nixosConfigurations.hidra-test = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = { inherit self; };
        modules = [
          "${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
          { nixpkgs.overlays = [ rust-overlay.overlays.default ]; }
          ./e2e/platform/linux/test-vm.nix
        ];
      };

      checks = forAllSystems (
        pkgs:
        {
          build = self.packages.${pkgs.stdenv.hostPlatform.system}.default;

          fmt =
            pkgs.runCommand "hidra-fmt"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                ];
              }
              ''
                export HOME=$TMPDIR
                # Each path is its own workspace, so fmt-check them individually.
                for ws in "" e2e e2e/targets/webhid e2e/targets/webhid/fixture; do
                  ( cd ${self}/$ws && cargo fmt --check )
                done
                touch $out
              '';
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.isDarwin {
          macos-virtual-signed = macosVirtualSigned pkgs;
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
          ];
          # libudev for nusb's enumeration on Linux.
          nativeBuildInputs = nixpkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
          buildInputs = nixpkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.udev ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
