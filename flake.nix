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

        src = craneLib.cleanCargoSource ./.;

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        version = cargoToml.workspace.package.version or "0.0.26";

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
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        bitty = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          cargoExtraArgs = "-p bitty-terminal";
        });
      in
      {
        packages = {
          default = bitty;
          bitty = bitty;
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
