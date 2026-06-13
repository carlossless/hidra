{
  description = "hidra: pure-Rust HID library with native, nusb and WebHID backends";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      cargoToml = nixpkgs.lib.importTOML ./Cargo.toml;
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;

          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          buildFeatures = [ "nusb" ];
          cargoBuildFlags = [ "--examples" ];

          # The crate is a library; install the example binaries so the
          # package has a runnable output (and the build exercises linking
          # against the platform HID frameworks).
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
      });

      checks = forAllSystems (pkgs: {
        # Builds examples and runs the test suite (buildRustPackage checkPhase).
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
              cd ${self}
              HOME=$TMPDIR cargo fmt --check
              touch $out
            '';
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
