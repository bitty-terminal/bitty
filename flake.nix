{
  description = "Bitty terminal - minimal correct terminal workspace";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.stable."1.97.1".minimal.override {
          targets = [ "x86_64-unknown-linux-gnu" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Filter source to keep bounded - only include necessary files, no unbounded.
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            let
              baseName = baseNameOf (toString path);
            in
              !(pkgs.lib.hasSuffix ".git" baseName) &&
              !(baseName == "target") &&
              !(baseName == ".worktrees") &&
              !(baseName == "dist") &&
              !(baseName == "result");
        };

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        version = cargoToml.workspace.package.version or "0.0.1";

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
          # Only build bitty-app binary, keep bounded.
          cargoExtraArgs = "-p bitty-app";
          # No unsafe - already denied via workspace lints.
        });
      in
      {
        packages = {
          default = bitty;
          bitty = bitty;
        };

        checks = {
          inherit bitty;
          # flake check will build bitty and ensure no extra checks fail.
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = with pkgs; [
            rustToolchain
            pkg-config
            fontconfig
            freetype
            # For nfpm validation in dev shell
            # nfpm is Go-based, not Nix - handled via CI matrix
          ];
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
