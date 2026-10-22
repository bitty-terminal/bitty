# Nix and NixOS (Flake and Cachix)

Bitty provides an official Nix flake at the repository root (`flake.nix`), built
using `crane` and `rust-overlay` with explicit Wayland, X11, Fontconfig, and
Freetype graphical dependencies.

Prebuilt binary closures are published to the official Cachix binary cache:

```text
https://bitty.cachix.org
```

Public key:

```text
bitty.cachix.org-1:yglUvUc8ehSxMCnU5t5p7Rer56pGE+TNI7/pWbWDdmc=
```

## User Installation and Execution

### Direct Execution (no install needed)

Run Bitty directly from GitHub with automatic binary cache resolution:

```sh
nix run github:bitty-terminal/bitty
```

### Profile Installation

Install Bitty into your user profile:

```sh
nix profile install github:bitty-terminal/bitty
```

### Binary Cache Acceleration (Cachix)

The `flake.nix` includes `nixConfig` to automatically request the binary cache.
You can also enable system-wide trusted substitution:

```sh
cachix use bitty
```

### NixOS System Configuration

Add Bitty to your flake inputs:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    bitty.url = "github:bitty-terminal/bitty";
  };

  outputs = { self, nixpkgs, bitty, ... }: {
    nixosConfigurations.myhostname = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ({ pkgs, ... }: {
          environment.systemPackages = [
            bitty.packages.${pkgs.system}.default
          ];
        })
      ];
    };
  };
}
```

## Continuous Delivery Pipeline

During release publishing (`.github/workflows/release.yml`), the `nix` job builds
the flake and uploads precompiled store closures directly to Cachix using
`cachix/cachix-action` under `environment: release`:

```yaml
- uses: cachix/install-nix-action@08dcb3a5e62fa31e2da3d490afc4176ef55ecd72 # v30
  with:
    nix_path: nixpkgs=channel:nixos-unstable
- uses: cachix/cachix-action@ad2ddac53f961de1989924296a1f236fcfbaa4fc # v15
  with:
    name: bitty
    authToken: ${{ secrets.CACHIX_AUTH_TOKEN }}
- run: nix build .#bitty --print-build-logs
```

This guarantees zero compile time on client machines when fetching supported tags.
