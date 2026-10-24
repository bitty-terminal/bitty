# Debian, Ubuntu, and Derivatives (APT)

Debian, Ubuntu, Linux Mint, Pop!_OS, and Kali Linux use the APT package manager.
Bitty distributes official `.deb` packages via a high-performance, GPG-signed
APT static repository hosted on Cloudflare R2 and served through the global CDN:

```text
https://cdn.bitty.run/apt/
```

- **Supported Architectures**: `amd64` (x86_64) and `arm64` (AArch64)
- **Supported Releases**: Ubuntu 24.04 (Noble, verified via release `verify-install` leg).
  Other Debian and Ubuntu derivatives (Debian 12, Ubuntu 22.04, Linux Mint, Kali,
  Pop!_OS) remain best-effort depending on glibc runtime compatibility.

## User Installation

### Method 1: One-Line Setup (Recommended)

Run the official setup script to install the GPG keyring and configure the APT source:

```sh
curl -fsSL https://cdn.bitty.run/apt/setup.sh | sudo bash
sudo apt install -y bitty
```

### Method 2: Modern deb822 Setup (Debian 11+ / Ubuntu 20.04+)

For automated systems or manual configuration without piping a script into bash:

```sh
# 1. Install the GPG signing keyring
sudo mkdir -p -m 0755 /etc/apt/keyrings
curl -fsSL https://cdn.bitty.run/apt/bitty-archive-keyring.gpg | sudo tee /etc/apt/keyrings/bitty-archive-keyring.gpg > /dev/null
sudo chmod 0644 /etc/apt/keyrings/bitty-archive-keyring.gpg

# 2. Add the deb822 source list
curl -fsSL https://cdn.bitty.run/apt/bitty.sources | sudo tee /etc/apt/sources.list.d/bitty.sources > /dev/null
sudo chmod 0644 /etc/apt/sources.list.d/bitty.sources

# 3. Update cache and install
sudo apt update
sudo apt install -y bitty
```

### Method 3: Traditional `sources.list.d` Setup

If your tooling prefers one-line repository declarations:

```sh
sudo mkdir -p -m 0755 /etc/apt/keyrings
curl -fsSL https://cdn.bitty.run/apt/bitty-archive-keyring.gpg | sudo tee /etc/apt/keyrings/bitty-archive-keyring.gpg > /dev/null
sudo chmod 0644 /etc/apt/keyrings/bitty-archive-keyring.gpg

echo "deb [signed-by=/etc/apt/keyrings/bitty-archive-keyring.gpg] https://cdn.bitty.run/apt stable main" | sudo tee /etc/apt/sources.list.d/bitty.list
sudo chmod 0644 /etc/apt/sources.list.d/bitty.list

sudo apt update
sudo apt install -y bitty
```

## Security and Verification

The repository metadata (`InRelease` and `Release.gpg`) is signed on every release with
the dedicated Bitty APT Archive GPG key:

- **Key Fingerprint**: `BCB87D7B 489A 6416 8080 D900 DF47 51B6 EA71 1CB8`
- **Key UID**: `Bitty APT Archive <maintainers@bitty-terminal.dev>`
- **Key Type**: RSA 4096-bit

APT verifies the cryptographic signature of `InRelease` against `/etc/apt/keyrings/bitty-archive-keyring.gpg`
before accepting package updates.

## Release Automation and Structure

The release workflow (`.github/workflows/release.yml`) builds `.deb` packages via
`nfpm` for both `x86_64` and `aarch64`. The `apt` job runs under `environment: release`
and:

1. Organizes packages into standard Debian pool layout (`pool/main/b/bitty/bitty_<version>_<arch>.deb`).
2. Generates architecture-specific `Packages` and `Packages.gz` via `scripts/generate-apt-repo.sh`.
3. Creates the `Release` manifest, computes checksums, and signs `InRelease` and `Release.gpg`
   using the secret `APT_GPG_PRIVATE_KEY`. The `scripts/import-apt-gpg-key.sh`
   helper accepts armored, base64-encoded, quoted/BOM/CRLF, and literal-`\n` or
   double-escaped secret forms (including an armored block embedded in text),
   and fails closed unless the imported secret-key fingerprint matches
   `BCB87D7B489A64168080D900DF4751B6EA711CB8` before anything is signed. On
   failure it prints a bounded shape diagnostic (length, armor presence, UTF-8
   validity, base64 decode status/length, OpenPGP packet magic) and never the
   secret value.
4. Uploads the repository tree, keyring, deb822 source file, and setup script to Cloudflare R2
   at `bitty/apt/` via `wrangler r2 object put`.
