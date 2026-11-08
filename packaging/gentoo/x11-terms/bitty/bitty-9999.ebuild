# Copyright 2026 Bitty Authors
# Distributed under the terms of the Apache License 2.0

EAPI=8

# Workspace MSRV (Cargo.toml [workspace.package] rust-version) and edition 2024
# both need >= 1.85. rust-toolchain.toml pins 1.98.1 for developers, but plain
# cargo from virtual/rust ignores that file.
RUST_MIN_VER="1.85"

# Live ebuild only. The workspace resolves its internal crates through path
# dependencies, so a crates.io `CRATES=` list can never describe it: an empty
# CRATES makes cargo_live_src_unpack die outside PV=9999, and a non-empty list
# is impossible to generate. The tag-pinned, reproducible Gentoo package is
# x11-terms/bitty-bin; this ebuild tracks main for source builds from the
# overlay.
inherit cargo desktop git-r3 xdg

DESCRIPTION="GPU-accelerated terminal emulator with built-in pane multiplexer and Lua extensions"
HOMEPAGE="https://bitty.run"
EGIT_REPO_URI="https://github.com/bitty-terminal/bitty.git"
EGIT_BRANCH="main"

LICENSE="MIT OR Apache-2.0"
SLOT="0"
# Live ebuild: never keyworded (Gentoo devmanual).
KEYWORDS=""
# OpenRC is the default profile; the per-user daemon unit only makes sense
# under a systemd user manager, so it is gated instead of always installed.
IUSE="systemd"

# Direct NEEDED sonames only, aligned with packaging/linux/runtime-deps.toml:
# libfontconfig.so.1 and libfreetype.so.6. libgcc_s.so.1 comes from the
# sys-devel/gcc in @system, libc/libm from the base system. The X11, Wayland,
# and Vulkan backends are dlopen()ed at runtime and are not NEEDED entries, so
# they are documented in packaging/gentoo.md instead of over-declared here.
RDEPEND="
	media-libs/fontconfig:=
	media-libs/freetype:=
"
DEPEND="${RDEPEND}"
BDEPEND="
	sys-libs/ncurses
	virtual/pkgconfig
	virtual/rust
"

QA_FLAGS_IGNORED="usr/bin/bitty usr/bin/bittyd"

src_unpack() {
	git-r3_src_unpack
	cargo_live_src_unpack
}

src_compile() {
	# `bittyd` is a separate workspace package (crates/bitty-daemon), so it
	# needs its own package selection; both binaries ship from this ebuild.
	cargo_src_compile -p bitty-terminal --bin bitty -p bitty-daemon --bin bittyd
}

src_test() {
	cargo_src_test -p bitty-terminal --bin bitty
	cargo_src_test -p bitty-daemon
}

src_install() {
	cargo_src_install -p bitty-terminal --bin bitty -p bitty-daemon --bin bittyd

	# OpenRC profiles have no systemd user manager: users supervise
	# `bittyd start` themselves (or write an OpenRC service).
	if use systemd; then
		insinto /usr/lib/systemd/user
		doins packaging/systemd/bittyd.service
	fi

	domenu packaging/run.bitty.Bitty.desktop
	insinto /usr/share/metainfo
	doins packaging/run.bitty.Bitty.metainfo.xml

	for size in 16 32 64 128 256 512; do
		newicon -s ${size} packaging/icons/hicolor/${size}x${size}/apps/bitty.png bitty.png
	done
	doicon -s scalable packaging/icons/hicolor/scalable/apps/bitty.svg

	dodoc README.md CHANGELOG.md

	# tic ships in sys-libs/ncurses (BDEPEND); there is no sys-apps/tic atom.
	dodir /usr/share/terminfo
	tic -x -o "${ED}/usr/share/terminfo" terminfo/bitty.ti || die "tic failed"
}
