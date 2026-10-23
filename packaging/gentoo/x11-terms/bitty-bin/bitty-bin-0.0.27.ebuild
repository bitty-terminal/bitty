# Copyright 2026 Bitty Authors
# Distributed under the terms of the Apache License 2.0

EAPI=8

inherit desktop xdg

DESCRIPTION="GPU-accelerated terminal emulator (official prebuilt binary)"
HOMEPAGE="https://bitty.run"
SRC_URI="
	amd64? ( https://cdn.bitty.run/releases/v${PV}/bitty-${PV}-x86_64-unknown-linux-gnu.tar.zst )
	arm64? ( https://cdn.bitty.run/releases/v${PV}/bitty-${PV}-aarch64-unknown-linux-gnu.tar.zst )
"

LICENSE="MIT OR Apache-2.0"
SLOT="0"
KEYWORDS="-* ~amd64 ~arm64"
IUSE=""

# Direct NEEDED sonames only, aligned with packaging/linux/runtime-deps.toml:
# libfontconfig.so.1 and libfreetype.so.6. libgcc_s.so.1 comes from the
# sys-devel/gcc in @system, libc/libm from the base system. The X11, Wayland,
# and Vulkan backends are dlopen()ed at runtime and are not NEEDED entries, so
# they are documented in packaging/gentoo.md instead of over-declared here.
RDEPEND="
	media-libs/fontconfig:=
	media-libs/freetype:=
	!x11-terms/bitty
"
BDEPEND="
	app-arch/zstd
	sys-libs/ncurses
"

QA_PREBUILT="usr/bin/bitty"

src_unpack() {
	default
	if use amd64; then
		S="${WORKDIR}/bitty-${PV}-x86_64-unknown-linux-gnu"
	elif use arm64; then
		S="${WORKDIR}/bitty-${PV}-aarch64-unknown-linux-gnu"
	fi
}

src_install() {
	cd "${S}" || die

	dobin bin/bitty

	domenu share/applications/run.bitty.Bitty.desktop
	insinto /usr/share/metainfo
	doins share/metainfo/run.bitty.Bitty.metainfo.xml

	for size in 16 32 64 128 256 512; do
		newicon -s ${size} share/icons/hicolor/${size}x${size}/apps/bitty.png bitty.png
	done
	doicon -s scalable share/icons/hicolor/scalable/apps/bitty.svg

	dodoc README.md CHANGELOG.md

	dodir /usr/share/terminfo
	tic -x -o "${ED}/usr/share/terminfo" terminfo/bitty.ti || die "tic failed"
}
