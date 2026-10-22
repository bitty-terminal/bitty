#!/usr/bin/env python3
"""Generate a Debian/Ubuntu APT repository tree from .deb artifacts.

Reads built .deb packages, organizes them into the standard Debian pool layout
(pool/main/b/bitty/), generates Packages and Packages.gz indices for each
supported architecture, creates Release files, and optionally signs InRelease
and Release.gpg with GPG.

Zero external Python dependencies; works with Python 3.8+ standard library.
"""

from __future__ import annotations

import argparse
import email.utils
import gzip
import hashlib
import io
import os
import pathlib
import shutil
import subprocess
import tarfile


def parse_ar(data: bytes) -> dict[str, bytes]:
    """Parse a Debian .deb file (ar format) and return its member files."""
    magic = data[:8]
    if magic != b"!<arch>\n":
        raise ValueError(f"Invalid ar magic header: {magic!r}")

    members: dict[str, bytes] = {}
    offset = 8
    total_len = len(data)

    while offset < total_len:
        if offset + 60 > total_len:
            break
        header = data[offset : offset + 60]
        offset += 60

        name = header[:16].decode("ascii", errors="replace").strip().rstrip("/")
        size_str = header[48:58].decode("ascii", errors="replace").strip()
        try:
            size = int(size_str)
        except ValueError as exc:
            raise ValueError(
                f"Invalid size in ar header for member '{name}': {size_str}"
            ) from exc

        member_data = data[offset : offset + size]
        offset += size
        # ar members are 2-byte aligned
        if size % 2 == 1:
            offset += 1

        members[name] = member_data

    return members


def extract_control_file(deb_path: pathlib.Path) -> str:
    """Extract and decode the 'control' file from a .deb package."""
    deb_bytes = deb_path.read_bytes()
    members = parse_ar(deb_bytes)

    control_tar_bytes = None
    for member_name, data in members.items():
        if member_name.startswith("control.tar"):
            control_tar_bytes = data
            break

    if control_tar_bytes is None:
        raise ValueError(f"No control.tar.* found in {deb_path}")

    # Use tarfile to open the compressed tar bytes
    with tarfile.open(fileobj=io.BytesIO(control_tar_bytes)) as tar:
        for member in tar.getmembers():
            base_name = os.path.basename(member.name)
            if base_name == "control":
                extracted = tar.extractfile(member)
                if extracted is not None:
                    return extracted.read().decode("utf-8", errors="replace")

    raise ValueError(f"No 'control' member found inside control.tar of {deb_path}")


def compute_hashes_and_size(path: pathlib.Path) -> tuple[str, str, str, int]:
    """Return (md5, sha1, sha256, size_in_bytes) for the given file."""
    md5 = hashlib.md5()
    sha1 = hashlib.sha1()
    sha256 = hashlib.sha256()
    size = 0

    with path.open("rb") as f:
        while chunk := f.read(65536):
            md5.update(chunk)
            sha1.update(chunk)
            sha256.update(chunk)
            size += len(chunk)

    return md5.hexdigest(), sha1.hexdigest(), sha256.hexdigest(), size


def parse_control_fields(control_text: str) -> dict[str, str]:
    """Parse debian control file into key-value pairs, preserving multiline values."""
    fields: dict[str, str] = {}
    current_key: str | None = None
    current_value_lines: list[str] = []

    for line in control_text.splitlines():
        if not line:
            continue
        if line.startswith((" ", "\t")):
            if current_key:
                current_value_lines.append(line)
        else:
            if current_key:
                fields[current_key] = "\n".join(current_value_lines)
            parts = line.split(":", 1)
            current_key = parts[0].strip()
            current_value_lines = [parts[1].strip()] if len(parts) > 1 else [""]

    if current_key:
        fields[current_key] = "\n".join(current_value_lines)

    return fields


def determine_deb_arch(fields: dict[str, str], filename: str) -> str:
    """Determine the canonical APT architecture (amd64, arm64)."""
    arch = fields.get("Architecture", "").strip()
    if arch in ("amd64", "arm64", "all"):
        return arch
    if "x86_64" in filename or "amd64" in filename:
        return "amd64"
    if "aarch64" in filename or "arm64" in filename:
        return "arm64"
    raise ValueError(
        f"Unrecognized Debian architecture for package '{filename}' (Architecture: '{arch}')"
    )


def format_package_stanza(
    fields: dict[str, str],
    rel_filename: str,
    size: int,
    md5: str,
    sha1: str,
    sha256: str,
) -> str:
    """Format one package stanza for the Packages index."""
    lines: list[str] = []
    # Standard field order preferred by APT
    preferred_order = [
        "Package",
        "Version",
        "Architecture",
        "Maintainer",
        "Installed-Size",
        "Depends",
        "Recommends",
        "Suggests",
        "Conflicts",
        "Breaks",
        "Replaces",
        "Provides",
        "Section",
        "Priority",
        "Homepage",
        "Description",
    ]

    emitted_keys = set()
    for key in preferred_order:
        if key in fields:
            lines.append(f"{key}: {fields[key]}")
            emitted_keys.add(key)

    # Any remaining fields from control
    for key, val in fields.items():
        if key not in emitted_keys:
            lines.append(f"{key}: {val}")

    lines.append(f"Filename: {rel_filename}")
    lines.append(f"Size: {size}")
    lines.append(f"SHA256: {sha256}")
    lines.append(f"SHA1: {sha1}")
    lines.append(f"MD5sum: {md5}")

    return "\n".join(lines)


def generate_repo(
    dist_dir: pathlib.Path,
    repo_dir: pathlib.Path,
    suites: list[str],
    components: list[str],
    origin: str,
    label: str,
    description: str,
    gpg_key: str | None = None,
    assets_dir: pathlib.Path | None = None,
) -> None:
    """Generate the full APT repository structure."""
    repo_dir.mkdir(parents=True, exist_ok=True)
    pool_dir = repo_dir / "pool" / "main" / "b" / "bitty"
    pool_dir.mkdir(parents=True, exist_ok=True)

    # Collect .deb files from dist_dir or existing pool
    deb_candidates: list[pathlib.Path] = []
    if dist_dir.is_dir():
        deb_candidates.extend(dist_dir.glob("**/*.deb"))

    # Also check if debs were already in pool_dir
    for existing in pool_dir.glob("*.deb"):
        if existing not in deb_candidates:
            deb_candidates.append(existing)

    if not deb_candidates:
        raise ValueError(f"No .deb packages found in {dist_dir} or {pool_dir}")

    # Process and copy packages into pool with canonical names
    # e.g. bitty_<version>_<arch>.deb
    arch_packages: dict[str, list[str]] = {
        "amd64": [],
        "arm64": [],
    }

    for deb_file in sorted(deb_candidates):
        try:
            control_text = extract_control_file(deb_file)
        except (ValueError, OSError, tarfile.TarError) as exc:
            raise ValueError(
                f"Failed to extract control from package '{deb_file}': {exc}"
            ) from exc

        fields = parse_control_fields(control_text)
        pkg_name = fields.get("Package", "bitty")
        version = fields.get("Version", "0.0.1")
        arch = determine_deb_arch(fields, deb_file.name)

        if arch not in arch_packages:
            arch_packages[arch] = []

        canonical_name = f"{pkg_name}_{version}_{arch}.deb"
        target_deb = pool_dir / canonical_name

        if deb_file.resolve() != target_deb.resolve():
            shutil.copy2(deb_file, target_deb)
            print(f"Staged {deb_file.name} -> {target_deb.relative_to(repo_dir)}")

        md5, sha1, sha256, size = compute_hashes_and_size(target_deb)
        rel_filename = f"pool/main/b/bitty/{canonical_name}"
        stanza = format_package_stanza(fields, rel_filename, size, md5, sha1, sha256)
        arch_packages[arch].append(stanza)

    # Copy static assets (keyring, sources, setup.sh) if available
    if assets_dir and assets_dir.is_dir():
        for asset_name in ("bitty-archive-keyring.gpg", "bitty.sources", "setup.sh"):
            src_asset = assets_dir / asset_name
            if src_asset.is_file():
                dest_asset = repo_dir / asset_name
                shutil.copy2(src_asset, dest_asset)
                print(f"Copied asset: {asset_name} -> {repo_dir}")

    # Generate dists/<suite>/ for each suite (e.g. stable, noble, jammy, bookworm)
    utc_date = email.utils.formatdate(timeval=None, localtime=False, usegmt=True)

    for suite in suites:
        dists_suite_dir = repo_dir / "dists" / suite
        dists_suite_dir.mkdir(parents=True, exist_ok=True)

        for comp in components:
            for arch, stanzas in arch_packages.items():
                if not stanzas:
                    continue
                bin_dir = dists_suite_dir / comp / f"binary-{arch}"
                bin_dir.mkdir(parents=True, exist_ok=True)

                packages_file = bin_dir / "Packages"
                packages_content = "\n\n".join(stanzas) + "\n"
                packages_file.write_text(packages_content, encoding="utf-8")

                # Generate Packages.gz
                packages_gz_file = bin_dir / "Packages.gz"
                with gzip.open(packages_gz_file, "wb", compresslevel=9) as gz_out:
                    gz_out.write(packages_content.encode("utf-8"))

        # Generate dists/<suite>/Release
        release_file = dists_suite_dir / "Release"
        active_arches = [arch for arch, stanzas in arch_packages.items() if stanzas]

        # Scan all files under dists_suite_dir (excluding Release, InRelease, Release.gpg)
        index_files: list[pathlib.Path] = []
        for path in sorted(dists_suite_dir.rglob("*")):
            if path.is_file() and path.name not in (
                "Release",
                "InRelease",
                "Release.gpg",
            ):
                index_files.append(path)

        md5_entries: list[str] = []
        sha1_entries: list[str] = []
        sha256_entries: list[str] = []

        for ifile in index_files:
            rel_path = ifile.relative_to(dists_suite_dir).as_posix()
            f_md5, f_sha1, f_sha256, f_size = compute_hashes_and_size(ifile)
            md5_entries.append(f" {f_md5} {f_size:16d} {rel_path}")
            sha1_entries.append(f" {f_sha1} {f_size:16d} {rel_path}")
            sha256_entries.append(f" {f_sha256} {f_size:16d} {rel_path}")

        release_lines = [
            f"Origin: {origin}",
            f"Label: {label}",
            f"Suite: {suite}",
            f"Codename: {suite}",
            f"Date: {utc_date}",
            f"Architectures: {' '.join(active_arches)}",
            f"Components: {' '.join(components)}",
            f"Description: {description}",
            "MD5Sum:",
            *md5_entries,
            "SHA1:",
            *sha1_entries,
            "SHA256:",
            *sha256_entries,
        ]

        release_text = "\n".join(release_lines) + "\n"
        release_file.write_text(release_text, encoding="utf-8")
        print(f"Generated Release index: {release_file.relative_to(repo_dir)}")

        # Optional GPG signing
        if gpg_key:
            inrelease_file = dists_suite_dir / "InRelease"
            release_gpg_file = dists_suite_dir / "Release.gpg"

            print(f"Signing Release for suite {suite} with key: {gpg_key}")
            # InRelease (clearsign)
            cmd_inrelease = [
                "gpg",
                "--batch",
                "--yes",
                "--armor",
                "--clearsign",
                "-u",
                gpg_key,
                "-o",
                str(inrelease_file),
                str(release_file),
            ]
            subprocess.run(cmd_inrelease, check=True)

            # Release.gpg (detached signature)
            cmd_detach = [
                "gpg",
                "--batch",
                "--yes",
                "--armor",
                "--detach-sign",
                "-u",
                gpg_key,
                "-o",
                str(release_gpg_file),
                str(release_file),
            ]
            subprocess.run(cmd_detach, check=True)
            print(f"Created InRelease and Release.gpg for {suite}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--dist-dir",
        type=pathlib.Path,
        default=pathlib.Path("dist"),
        help="Directory with built .deb files",
    )
    parser.add_argument(
        "--repo-dir",
        type=pathlib.Path,
        default=pathlib.Path("apt"),
        help="Target repository directory",
    )
    parser.add_argument(
        "--suites",
        default="stable,bookworm,trixie,jammy,noble",
        help="Comma-separated suite names (default: stable,bookworm,trixie,jammy,noble)",
    )
    parser.add_argument(
        "--components",
        default="main",
        help="Comma-separated component names (default: main)",
    )
    parser.add_argument(
        "--origin",
        default="bitty-terminal",
        help="Origin header (default: bitty-terminal)",
    )
    parser.add_argument(
        "--label", default="Bitty", help="Label header (default: Bitty)"
    )
    parser.add_argument(
        "--description",
        default="Bitty terminal emulator APT repository",
        help="Description header",
    )
    parser.add_argument(
        "--gpg-key", default=None, help="GPG key ID or email to sign Release files"
    )
    parser.add_argument(
        "--assets-dir",
        type=pathlib.Path,
        default=pathlib.Path("packaging/apt"),
        help="Directory containing static assets (keyring, sources, setup.sh)",
    )

    args = parser.parse_args()

    suites = [s.strip() for s in args.suites.split(",") if s.strip()]
    components = [c.strip() for c in args.components.split(",") if c.strip()]
    gpg_key = args.gpg_key or os.environ.get("APT_GPG_KEY")

    generate_repo(
        dist_dir=args.dist_dir,
        repo_dir=args.repo_dir,
        suites=suites,
        components=components,
        origin=args.origin,
        label=args.label,
        description=args.description,
        gpg_key=gpg_key,
        assets_dir=args.assets_dir,
    )
    print("APT repository generation completed successfully.")


if __name__ == "__main__":
    main()
