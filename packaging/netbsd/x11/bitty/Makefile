# $NetBSD$
# NetBSD pkgsrc recipe for Bitty (issue #2624).
#
# The pkgsrc framework cannot build from an arbitrary source checkout; copy
# the package into a pkgsrc tree (e.g. /usr/pkgsrc/x11/bitty) and build:
#
#   cp -R packaging/netbsd/x11/bitty /usr/pkgsrc/x11/bitty
#   cd /usr/pkgsrc/x11/bitty
#   bmake makesum     # once the v0.0.27 tag exists; see distinfo
#   bmake install clean
#
# packaging/netbsd.md documents the workflow; packaging/README.md documents the
# support level (best-effort: no release job installs this package yet).
#
# Keep DISTNAME version in step with the workspace version in Cargo.toml.

DISTNAME=	bitty-0.0.27
CATEGORIES=	x11
MASTER_SITES=	${MASTER_SITE_GITHUB:=bitty-terminal/}
GITHUB_PROJECT=	bitty
GITHUB_TAG=	v${PKGVERSION_NOREV}

MAINTAINER=	maintainers@bitty-terminal.dev
HOMEPAGE=	https://bitty.run/
COMMENT=	GPU-accelerated terminal emulator with panes and Lua extensions
LICENSE=	mit OR apache-2.0

USE_TOOLS+=	pkg-config tic
USE_LANGUAGES=	c c++

CARGO_BUILD_ARGS+=	-p bitty-terminal --bin bitty

INSTALLATION_DIRS+=	bin
INSTALLATION_DIRS+=	share/applications
INSTALLATION_DIRS+=	share/metainfo
INSTALLATION_DIRS+=	share/terminfo
INSTALLATION_DIRS+=	share/doc/bitty
.for size in 16 32 64 128 256 512
INSTALLATION_DIRS+=	share/icons/hicolor/${size}x${size}/apps
.endfor
INSTALLATION_DIRS+=	share/icons/hicolor/scalable/apps

do-install:
	${INSTALL_PROGRAM} ${WRKSRC}/target/release/bitty \
		${DESTDIR}${PREFIX}/bin/bitty
	${INSTALL_DATA} ${WRKSRC}/packaging/run.bitty.Bitty.desktop \
		${DESTDIR}${PREFIX}/share/applications/run.bitty.Bitty.desktop
	${INSTALL_DATA} ${WRKSRC}/packaging/run.bitty.Bitty.metainfo.xml \
		${DESTDIR}${PREFIX}/share/metainfo/run.bitty.Bitty.metainfo.xml
.for size in 16 32 64 128 256 512
	${INSTALL_DATA} ${WRKSRC}/packaging/icons/hicolor/${size}x${size}/apps/bitty.png \
		${DESTDIR}${PREFIX}/share/icons/hicolor/${size}x${size}/apps/bitty.png
.endfor
	${INSTALL_DATA} ${WRKSRC}/packaging/icons/hicolor/scalable/apps/bitty.svg \
		${DESTDIR}${PREFIX}/share/icons/hicolor/scalable/apps/bitty.svg
	tic -x -o ${DESTDIR}${PREFIX}/share/terminfo ${WRKSRC}/terminfo/bitty.ti
	${INSTALL_DATA} ${WRKSRC}/README.md ${DESTDIR}${PREFIX}/share/doc/bitty/
	${INSTALL_DATA} ${WRKSRC}/CHANGELOG.md ${DESTDIR}${PREFIX}/share/doc/bitty/

.include "../../lang/rust/cargo.mk"
.include "../../graphics/freetype2/buildlink3.mk"
.include "../../fonts/fontconfig/buildlink3.mk"
.include "../../mk/bsd.pkg.mk"
