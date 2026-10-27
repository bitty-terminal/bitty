#
# openSUSE recipe for Bitty, built in the openSUSE Build Service (OBS).
#
# Sibling recipes: packaging/bitty.spec (Fedora/EPEL RPM layout used by nfpm),
# packaging/alpine/APKBUILD, packaging/gentoo/, packaging/void/srcpkgs/bitty/.
# This spec builds from the release source tree; the prebuilt RPM path stays in
# packaging/bitty.spec and packaging/fedora.md.
#
# Source0: the release source tarball for the v%{version} tag. The OBS source
# service (packaging/opensuse/_service, every service declared mode="disabled")
# produces the same `bitty-%{version}.tar.gz` from
# https://github.com/bitty-terminal/bitty.git at revision v%{version}; the URL
# below is the equivalent upstream archive and the fallback for a plain
# `rpmbuild -ba` outside osc. OBS records the digest in the checked-in
# `.sha256` source file, so source checksums are regenerated at tag time
# (`osc service disabledrun` / `osc addremove`), never hand-edited.
#
Name:           bitty
Version:        0.0.30
Release:        0
Summary:        GPU-accelerated terminal emulator with a pane multiplexer and Lua extensions
License:        MIT OR Apache-2.0
Group:          System/X11/Terminals
URL:            https://bitty.run
Source0:        https://github.com/bitty-terminal/bitty/archive/refs/tags/v%{version}.tar.gz#/%{name}-%{version}.tar.gz

# Runtime names follow openSUSE: the soname providers are libfontconfig1 and
# libfreetype6, not the Fedora/freetype spelling used in packaging/bitty.spec.
# rpm also generates the soname requires automatically; these keep the mapping
# explicit (packaging/linux/runtime-deps.toml maps the same libraries).
Provides:       bitty-terminal = %{version}-%{release}
Requires:       libfontconfig1
Requires:       libfreetype6

# Rust >= the workspace MSRV (rust-version in the root Cargo.toml). openSUSE
# Leap ships an older compiler than Tumbleweed, so the OBS project pulls the
# toolchain from devel:languages:rust; see packaging/opensuse/README.md.
BuildRequires:  cargo
BuildRequires:  rust >= 1.85
BuildRequires:  pkg-config
BuildRequires:  fontconfig-devel
BuildRequires:  freetype2-devel
BuildRequires:  zlib-devel
# tic(1) compiles terminfo/bitty.ti at package build time.
BuildRequires:  ncurses-utils

%description
Bitty is a programmable, microkernel terminal emulator built around a small
Rust core and a two-tiered extension architecture. One binary carries the
terminal, a pane multiplexer, a GPU-accelerated renderer, and Lua extensions.

This package installs the desktop entry, AppStream metainfo, the hicolor icon
set, and the compiled terminfo database next to the binary.

%prep
%autosetup -n %{name}-%{version}

%build
# Cargo.lock is committed and authoritative; cargo honors it by default and
# the release CI enforces divergence with `--locked`. Kept identical to the
# cargo invocation in packaging/bitty.spec.
cargo build --release -p bitty-terminal --bin bitty

%install
install -d %{buildroot}%{_bindir}
install -m 0755 target/release/bitty %{buildroot}%{_bindir}/bitty

install -d %{buildroot}%{_datadir}/applications
install -m 0644 packaging/run.bitty.Bitty.desktop \
    %{buildroot}%{_datadir}/applications/run.bitty.Bitty.desktop

install -d %{buildroot}%{_datadir}/metainfo
install -m 0644 packaging/run.bitty.Bitty.metainfo.xml \
    %{buildroot}%{_datadir}/metainfo/run.bitty.Bitty.metainfo.xml

for size in 16 32 64 128 256 512; do
    install -d %{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps
    install -m 0644 "packaging/icons/hicolor/${size}x${size}/apps/bitty.png" \
        "%{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps/bitty.png"
done
install -d %{buildroot}%{_datadir}/icons/hicolor/scalable/apps
install -m 0644 packaging/icons/hicolor/scalable/apps/bitty.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/bitty.svg

# Never install the raw terminfo source; compile it into the database.
install -d %{buildroot}%{_datadir}/terminfo
tic -x -o %{buildroot}%{_datadir}/terminfo terminfo/bitty.ti

%files
%license LICENSE
%doc README.md CHANGELOG.md
%{_bindir}/bitty
%{_datadir}/applications/run.bitty.Bitty.desktop
%{_datadir}/metainfo/run.bitty.Bitty.metainfo.xml
%{_datadir}/icons/hicolor/16x16/apps/bitty.png
%{_datadir}/icons/hicolor/32x32/apps/bitty.png
%{_datadir}/icons/hicolor/64x64/apps/bitty.png
%{_datadir}/icons/hicolor/128x128/apps/bitty.png
%{_datadir}/icons/hicolor/256x256/apps/bitty.png
%{_datadir}/icons/hicolor/512x512/apps/bitty.png
%{_datadir}/icons/hicolor/scalable/apps/bitty.svg
%{_datadir}/terminfo/b/bitty

%changelog
* Mon Oct 26 2026 Bitty Maintainers <maintainers@bitty-terminal.dev> - 0.0.30-0
- Release 0.0.30
* Fri Oct 23 2026 Bitty Maintainers <maintainers@bitty-terminal.dev> - 0.0.28-0
- Initial openSUSE package: source build via OBS, desktop entry, AppStream
  metainfo, hicolor icons, compiled terminfo.
