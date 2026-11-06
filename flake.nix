{
  description = "Bitty terminal - minimal correct terminal workspace";

  nixConfig = {
    extra-substituters = [
      "https://bitty.cachix.org"
      "https://cache.nixos.org"
    ];
    extra-trusted-public-keys = [
      "bitty.cachix.org-1:yglUvUc8ehSxMCnU5t5p7Rer56pGE+TNI7/pWbWDdmc="
      "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # `craneLib.cleanCargoSource` keeps only Rust sources plus Cargo
        # metadata (crane `filterCargoSources`: `.rs`, `.toml`, `Cargo.lock`,
        # `.cargo/config`), so every non-Rust file a crate compiles in is
        # dropped from the Nix build. Keep those assets on top of the Cargo
        # fileset:
        #   - crates/bitty-render/assets: OFL symbols face read by
        #     `include_bytes!` in `bitty-render/src/embedded.rs` (#2931)
        #   - terminfo/bitty.ti: terminfo repair source read by
        #     `include_str!` in `bitty-terminal/src/doctor.rs` (#2924)
        #   - crates/bitty-terminal/bitty.rc and packaging/icons/bitty.ico:
        #     Windows icon resource compiled by `crates/bitty-terminal/build.rs`
        #     on a Windows host (#2929)
        # Test fixtures stay out: both derivations set `doCheck = false`, so
        # cargo never compiles the test targets that read them.
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            (craneLib.fileset.commonCargoSources ./.)
            # crane's filter also keeps the extension-less `.cargo/config`.
            (pkgs.lib.fileset.maybeMissing ./.cargo/config)
            ./crates/bitty-render/assets
            ./terminfo/bitty.ti
            ./crates/bitty-terminal/bitty.rc
            ./packaging/icons/bitty.ico
          ];
        };

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        version = cargoToml.workspace.package.version or "0.0.31";

        commonArgs = {
          inherit src version;
          pname = "bitty";
          strictDeps = true;
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          buildInputs = with pkgs; [
            fontconfig
            freetype
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            xorg.libxcb
            xorg.libX11
            libxkbcommon
            wayland
          ];
          # #2591: the workspace `[patch."https://github.com/bitty-terminal/bitty"]`
          # table points the external `bitty-execution` dependency's
          # `bitty-pty`/`bitty-plugin-host` edges at the local crates. A build
          # tree assembled by crane's dummy-source mode replaces those crates
          # with empty stubs, so refuse it before cargo sees the graph.
          postPatch = ''
            for crate in bitty-pty bitty-plugin-host; do
              lib="crates/$crate/src/lib.rs"
              if [ ! -f "$lib" ]; then
                echo "error: $lib is missing; the workspace [patch] requires real sources (#2591)" >&2
                exit 1
              fi
              if grep -q "unused_extern_crates" "$lib" &&
                grep -q 'pub fn main() {}' "$lib"; then
                echo "error: $lib is a crane dummy stub; the workspace [patch] requires real sources (#2591)" >&2
                exit 1
              fi
            done
          '';
        };

        # #2591: keep this a single real-source derivation. `cargoArtifacts`
        # must stay explicitly null: crane's buildPackage otherwise creates a
        # dependency-only derivation from a stubbed workspace, which cannot
        # compile the external `bitty-execution` dependency against the local
        # `[patch]` crates. Release tags change the workspace version, so the
        # stub source never survives between releases anyway; Cachix caches
        # the final closure. `just nix-patch-test` fails if dummy-source
        # caching comes back while the patch paths exist.
        bitty = craneLib.buildPackage (commonArgs // {
          cargoArtifacts = null;
          # The 25-minute release job only needs the binary; compiling test
          # targets in this derivation (`cargo test --no-run`) adds no check
          # coverage because crane never runs them. The test suites run in CI.
          doCheck = false;
          cargoExtraArgs = "-p bitty-terminal";
        });
        # The session host is a separate binary in the same workspace. It is
        # built as its own derivation so a consumer can install either one.
        bittyd = craneLib.buildPackage (commonArgs // {
          cargoArtifacts = null;
          doCheck = false;
          cargoExtraArgs = "-p bitty-daemon";
        });
      in
      {
        packages = {
          default = bitty;
          bitty = bitty;
          bittyd = bittyd;
        };

        apps = {
          default = flake-utils.lib.mkApp {
            drv = bitty;
          };
          bitty = flake-utils.lib.mkApp {
            drv = bitty;
          };
        };

        checks = {
          inherit bitty;
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = with pkgs; [
            rustToolchain
            pkg-config
            fontconfig
            freetype
          ];
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
