# Extension install seed boundary (bitty#2399, W-72)

## Status

Implemented seed slice for `bitty#2399` under the accepted package-manager
boundary (`W-72`). This note records what Core keeps, what it no longer owns,
and what the registry/manager side must provide next. It is a Core-local
design note, not a canonical package-manager contract.

## Context

`bitty` ships one network-reaching bootstrap exception: `bitty extension
install` may fetch a prebuilt extension so `bitty-plugin-manager` itself can
be installed on first use. The seed exists only to break that bootstrap
cycle. It is not a package manager: it does not solve dependencies, select
versions, resolve catalogs, or present a marketplace.

The contract direction is the same one recorded in
`bitty-docs/docs/development/package-manager-boundary.md`: Core never fetches
or resolves package metadata, and install-time package management belongs to
the external manager. This note applies that direction to the seed without
weakening the existing integrity controls.

## Seed contract

- **Shape:** `bitty extension install <name> --version <X.Y.Z>`.
  `--version` is required; a bare install is a usage error (exit `2`). Core
  has no "latest" selector.
- **Identity:** exactly one `(name, version, host target)` per invocation.
  The target is detected, never a flag, so a foreign-target release cannot be
  requested by mistake.
- **Fetch:** fixed-argv system `curl` (pinned to `https://cdn.bitty.run`,
  `--proto =https`, no shell) plus system `tar` for extraction plus
  Rust-side SHA-256 verification. No Rust network stack is linked.
- **Lock record:** the release's published `SHA256SUMS` is the only lock Core
  reads. `extensions/catalog.v1` is not fetched, parsed, or embedded by Core.
- **Budget:** at most one release artifact plus its `SHA256SUMS` manifest per
  invocation. A verified local cache hit fetches nothing.
- **Offline fallback:** the seed attempts the manifest fetch before checking
  the cache. When that manifest fetch fails (offline, DNS/TLS, timeout,
  non-success HTTP), an already-installed exact version that verifies
  locally is reused with no download. Reuse is available only when both
  `--digest` and `--force` are absent; either flag keeps the full fetch path.
  A tarball transport failure after a parsed manifest does not qualify, and
  integrity, manifest-shape, and size failures never reach the cache.
- **Handoff:** after staging, the seed prints the installed executable and
  stops. Catalog resolution, latest selection, updates, and list/inspect
  presentation are `bitty-plugin-manager` work.

## Bounds

| Dimension         | Bound                                                      |
| ----------------- | ---------------------------------------------------------- |
| Seed identities   | 1 `(name, version, host target)` per invocation            |
| Name              | `[a-z][a-z0-9-]{0,31}`                                     |
| Version           | strict numeric `X.Y.Z` (no prerelease/build metadata)      |
| Target            | host triple grammar, at most 64 bytes                      |
| URLs              | at most 512 bytes, `https` on `cdn.bitty.run` only         |
| Manifest total    | at most 64 KiB, 4096 lines, 1024 bytes per line            |
| Tarball           | executable ceiling plus 1 MiB of descriptor/tar/gzip slack |
| Extracted members | exactly `bitty-component.toml` and `bitty-<name>`          |

Every bound is checked before the user tier is mutated. A failure exits `4`
for metadata/artifact-integrity failures and `1` for environmental failures.

## Trust chain

- Unsigned 0.1.0 verifies by hash only; Windows ships unsigned, so hash-only
  is the uniform story. Sigstore/cosign remains a follow-up.
- The seed allowlists the initial URL only. It follows `curl --location`
  redirects, so HTTPS does not pin the host and path across a redirect; the
  downloaded bytes are still checked against `SHA256SUMS`, so a redirect
  cannot substitute content without failing the hash gate.
- All-available-must-match: the `SHA256SUMS` entry and the optional
  caller-supplied `--digest` must each match the downloaded bytes. A
  mismatch refuses with exit `4`; there is no silent pick.
- The TOFU pin records the first verified digest per extension name. A later
  differing digest fails closed as a pin mismatch unless `--force` explicitly
  re-pins after verification. That refusal is also the seed's downgrade and
  substitution guard: an older or swapped artifact cannot silently replace a
  pinned release.
- Consent precedes every transport contact. A decline or EOF exits `1` with
  nothing fetched or staged; no digest can silently authorize a download.

## Catalog and lock ownership

Core no longer owns the extension catalog or the full lock:

- the published `extensions/catalog.v1` remains a release artifact generated
  from published `SHA256SUMS`; it is the manager's input, not Core's;
- release `SHA256SUMS` objects remain the immutable per-release lock records;
- `scripts/make-extension-catalog.sh` is retained as a compatibility
  publisher for release tooling during the migration. It is not called by
  Core at runtime and is not part of the seed path.

The registry/extension layer (`bitty-plugins`) is the intended owner of the
catalog and lock lifecycle. Core reads only the resolved release lock for the
single pinned artifact it installs.

## Migration

1. Users and automation that relied on a bare install must pass
   `--version X.Y.Z`. The usage diagnostic names the manager and the exact
   pinned form.
2. `bitty extension install <name> --version <X.Y.Z>` keeps working without a
   catalog. Pinned installs remain the offline path.
3. Once `bitty-plugin-manager` owns install/update/list, it resolves the
   catalog and passes an exact pinned release to the seed (or installs
   directly); Core keeps the seed only as the bootstrap fallback when the
   manager is absent.
4. The retired symbols are guarded by
   `crates/bitty-terminal/tests/extension_seed_boundary.rs`: re-adding a
   catalog parser, a latest selector, or a third `transport.fetch` site fails
   `just check` before review.

## Verification

- `crates/bitty-terminal/tests/extension_seed_boundary.rs` — retired symbols
  stay out of Core; the budget stays one pinned release; the seed opens no
  Rust network path.
- `crates/bitty-terminal/src/component.rs` unit tests — pinned install end to
  end, offline verified-cache reuse (manifest-only), tarball-outage refusal
  without cache reuse, tamper/pin-mismatch refusal, corrupt-pin recovery only
  with `--force`, and downgrade-with-differing-digest refusal.
- `crates/bitty-terminal/tests/cli_component.rs` — a bare install is usage
  (exit `2`) before any network contact; help names the manager boundary.
- `crates/bitty-terminal/tests/fetch_env.rs` — exactly two fetches
  (`SHA256SUMS` plus one tarball), no catalog URL, secret scrubbing intact,
  oversize refusal mapped to exit `4`.

## Cross-repository follow-up (bitty-plugins)

The registry/extension layer must own the refreshed catalog and enforce its
trust chain:

- publish a versioned, signed catalog snapshot with a monotonic
  `snapshot_version`, a bounded timestamp/expiry window, and a digest over
  the exact bytes;
- reject unknown fields instead of silently ignoring them, bound the total
  bytes, entries, and per-entry bytes, and refuse a snapshot older than the
  last accepted version (no downgrade);
- verify the signature and per-entry artifact digests before a version is
  offered, and fail closed with a clear diagnostic when the refreshed catalog
  fails;
- keep the seed entry (identity + pinned digest + target) as the only
  offline/bootstrap path, while the refreshed catalog is the normal
  resolution input.

Those changes belong to `bitty-plugins` and its manager task; this repository
does not implement or gate them.
