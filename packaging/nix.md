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

### Known Issue: `v0.0.27` Flake Lock

The published `v0.0.27` tag cannot be evaluated directly: its `flake.lock`
carries a stale nixpkgs `narHash`, so Nix aborts with
`NAR hash mismatch in input 'github:NixOS/nixpkgs/5adc5083...'` and the
`Publish Nix Flake (Cachix)` release job failed before publishing a closure.
The tag is immutable and stays exactly as published; the corrected lock ships
with `v0.0.28` (see <https://github.com/bitty-terminal/bitty/issues/2568>).

Until `v0.0.28`, override the nixpkgs input with the revision the tag already
pins, which makes Nix compute the hash instead of trusting the lock entry:

```sh
nix run --override-input nixpkgs github:NixOS/nixpkgs/5adc50831c259f87faeb09ddf930d43597197473 github:bitty-terminal/bitty/v0.0.27
```

```sh
nix profile install --override-input nixpkgs github:NixOS/nixpkgs/5adc50831c259f87faeb09ddf930d43597197473 github:bitty-terminal/bitty/v0.0.27
```

Flake consumers can bypass the broken node with `follows` instead:

```nix
inputs.bitty = {
  url = "github:bitty-terminal/bitty/v0.0.27";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

No Cachix closure exists for `v0.0.27`, so either workaround builds Bitty from
source (the nixpkgs dependencies still substitute from `cache.nixos.org`).

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

## Verifying the Flake Lock

`flake.lock` pins every input to an exact revision together with its NAR hash
and commit metadata. A stale hash or `lastModified` is only detected when Nix
resolves the lock, and the release job does that after the tag already exists,
so a broken lock can reach a release unopposed. Contributors who touch
`flake.nix` or `flake.lock` run:

```sh
just nix-lock-check
```

The gate resolves every locked input (`nix flake metadata`), evaluates
`.#packages.x86_64-linux.bitty` without building it — the evaluation is what
makes Nix verify every locked `narHash` and `lastModified` — and fails if
resolving the flake rewrites `flake.lock`. `just nix-lock-test` covers the
guard itself with a stub Nix and runs as part of `just check`. CI runs the same
check inside `Quality gates` whenever `flake.nix` or `flake.lock` changes; it
installs Nix, never builds the package, and never pushes to Cachix.

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
