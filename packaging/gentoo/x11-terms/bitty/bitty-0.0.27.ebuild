# Copyright 2026 Bitty Authors
# Distributed under the terms of the Apache License 2.0

EAPI=8

CRATES=""

inherit cargo desktop xdg

DESCRIPTION="GPU-accelerated terminal emulator with built-in pane multiplexer and Lua extensions"
HOMEPAGE="https://bitty.run"
SRC_URI="https://github.com/bitty-terminal/bitty/archive/refs/tags/v${PV}.tar.gz -> ${P}.tar.gz
	${CARGO_CRATE_URIS}"

LICENSE="Apache-2.0"
SLOT="0"
KEYWORDS="~amd64 ~arm64"
IUSE=""

RDEPEND="
	media-libs/fontconfig
	media-libs/freetype
	sys-libs/ncurses:0=
	x11-libs/libX11
	x11-libs/libxkbcommon
	dev-libs/wayland
"
DEPEND="${RDEPEND}"
BDEPEND="
	virtual/pkgconfig
	sys-apps/tic
"

QA_FLAGS_IGNORED="usr/bin/bitty"

src_unpack() {
	if [[ -n ${CARGO_CRATE_URIS} ]]; then
		cargo_src_unpack
	else
		default
		cargo_live_src_unpack
	fi
}

src_compile() {
	cargo_src_compile -p bitty-terminal --bin bitty
}

src_test() {
	cargo_src_test -p bitty-terminal --bin bitty
}

src_install() {
	cargo_src_install -p bitty-terminal --bin bitty

	domenu packaging/run.bitty.Bitty.desktop
	insinto /usr/share/metainfo
	doins packaging/run.bitty.Bitty.metainfo.xml

	for size in 16 32 64 128 256 512; do
		newicon -s ${size} packaging/icons/hicolor/${size}x${size}/apps/bitty.png bitty.png
	done
	doicon -s scalable packaging/icons/hicolor/scalable/apps/bitty.svg

	dodoc README.md CHANGELOG.md

	dodir /usr/share/terminfo
	tic -x -o "${ED}/usr/share/terminfo" terminfo/bitty.ti || die "tic failed"
}
