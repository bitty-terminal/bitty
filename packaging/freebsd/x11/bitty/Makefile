# FreeBSD Ports recipe for Bitty (issue #2512).
#
# The ports framework cannot build from a source checkout; copy the port into a
# ports tree (or a local overlays tree) and build it there:
#
#   cp -R packaging/freebsd/x11/bitty /usr/ports/x11/bitty
#   cd /usr/ports/x11/bitty
#   make makesum      # once the v0.0.29 tag exists; see distinfo
#   make install clean
#
# packaging/freebsd.md documents the workflow; packaging/README.md documents the
# support level (best-effort: no release job installs this port yet).
#
# Keep PORTVERSION in step with the workspace version in Cargo.toml.

PORTNAME=	bitty
PORTVERSION=	0.0.29
CATEGORIES=	x11

# Distfile is the released source tarball for the v${PORTVERSION} tag: the same
# archive the Alpine, Void, and Gentoo source recipes fetch. The CDN artifact
# that scripts/install.sh tells FreeBSD users to download
# (https://cdn.bitty.run/releases/v${PORTVERSION}/bitty-${PORTVERSION}-x86_64-unknown-freebsd.tar.xz)
# holds only bin/bitty plus the docs, so a port builds from source to install
# the terminfo entry and the desktop integration files.
MASTER_SITES=	https://github.com/bitty-terminal/bitty/archive/refs/tags/
DISTFILES=	v${PORTVERSION}${EXTRACT_SUFX}

MAINTAINER=	xuepo@bitty.run
COMMENT=	GPU-accelerated terminal emulator with panes and Lua extensions
WWW=		https://bitty.run

# Cargo.toml, nfpm.yaml, and packaging/PKGBUILD all declare "MIT OR Apache-2.0";
# the repository ships the MIT text as LICENSE, so only that file is pinned.
LICENSE=	MIT APACHE20
LICENSE_COMB=	dual
LICENSE_FILE_MIT=	${WRKSRC}/LICENSE

# Release CI builds and tests the FreeBSD target (x86_64-unknown-freebsd) on
# amd64 only; aarch64 has no build or smoke coverage, so it stays excluded
# until the release matrix covers it.
ONLY_FOR_ARCHS=	amd64
ONLY_FOR_ARCHS_REASON=	FreeBSD release CI builds and tests amd64 only (x86_64-unknown-freebsd)

# Direct NEEDED libraries of the release binary, matching
# packaging/linux/runtime-deps.toml (fontconfig, freetype). The X11, Wayland,
# and Vulkan backends are dlopen'd at runtime and ship with the desktop stack,
# so they are not hard runtime dependencies (same stance as the apk, whose
# depends are fontconfig + freetype).
LIB_DEPENDS=	libfontconfig.so:x11-fonts/fontconfig \
		libfreetype.so:print/freetype2
# tic compiles terminfo/bitty.ti at install time; devel/ncurses is the port that
# carries it (cargo.mk adds cmake from the `cmake` crate entry on its own).
BUILD_DEPENDS=	${LOCALBASE}/bin/tic:devel/ncurses

USES=		cargo desktop-file-utils pkgconfig

# Build only the binary; the workspace also carries test-support crates that a
# package does not need.
CARGO_BUILD_ARGS+=	-p bitty-terminal --bin bitty

do-install:
	@${MKDIR} ${STAGEDIR}${PREFIX}/bin
	${INSTALL_PROGRAM} ${CARGO_TARGET_DIR}/release/bitty \
		${STAGEDIR}${PREFIX}/bin/bitty
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/applications
	${INSTALL_DATA} ${WRKSRC}/packaging/run.bitty.Bitty.desktop \
		${STAGEDIR}${PREFIX}/share/applications/run.bitty.Bitty.desktop
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/metainfo
	${INSTALL_DATA} ${WRKSRC}/packaging/run.bitty.Bitty.metainfo.xml \
		${STAGEDIR}${PREFIX}/share/metainfo/run.bitty.Bitty.metainfo.xml
.for size in 16 32 64 128 256 512
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/icons/hicolor/${size}x${size}/apps
	${INSTALL_DATA} ${WRKSRC}/packaging/icons/hicolor/${size}x${size}/apps/bitty.png \
		${STAGEDIR}${PREFIX}/share/icons/hicolor/${size}x${size}/apps/bitty.png
.endfor
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/icons/hicolor/scalable/apps
	${INSTALL_DATA} ${WRKSRC}/packaging/icons/hicolor/scalable/apps/bitty.svg \
		${STAGEDIR}${PREFIX}/share/icons/hicolor/scalable/apps/bitty.svg
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/terminfo
	${LOCALBASE}/bin/tic -x -o ${STAGEDIR}${PREFIX}/share/terminfo \
		${WRKSRC}/terminfo/bitty.ti
	@${MKDIR} ${STAGEDIR}${DOCSDIR}
	${INSTALL_DATA} ${WRKSRC}/README.md ${WRKSRC}/CHANGELOG.md ${STAGEDIR}${DOCSDIR}

# CARGO_CRATES below is `make cargo-crates` output: run it again (and re-run
# `make makesum`) whenever Cargo.lock changes, then commit the refreshed list.

CARGO_CRATES=	ab_glyph-0.2.32 \
	ab_glyph_rasterizer-0.1.10 \
	adler2-2.0.1 \
	ahash-0.8.12 \
	allocator-api2-0.2.21 \
	android-activity-0.6.1 \
	android-properties-0.2.2 \
	android_system_properties-0.1.6 \
	anyhow-1.0.104 \
	arboard-3.6.1 \
	arrayref-0.3.9 \
	arrayvec-0.7.8 \
	as-raw-xcb-connection-1.0.1 \
	ash-0.38.0+1.3.281 \
	atomic-waker-1.1.2 \
	autocfg-1.5.1 \
	aws-lc-rs-1.18.1 \
	aws-lc-sys-0.45.0 \
	base16ct-1.0.0 \
	bit-set-0.8.0 \
	bit-vec-0.8.0 \
	bitflags-1.3.2 \
	bitflags-2.13.2 \
	block-0.1.6 \
	block-buffer-0.12.1 \
	block2-0.5.1 \
	bumpalo-3.20.3 \
	bytemuck-1.25.2 \
	bytemuck_derive-1.12.1 \
	byteorder-lite-0.1.0 \
	bytes-1.12.1 \
	calloop-0.13.0 \
	calloop-wayland-source-0.3.0 \
	cc-1.5.1 \
	cfg-if-1.0.5 \
	cfg_aliases-0.1.1 \
	cfg_aliases-0.2.2 \
	chrono-0.4.45 \
	clipboard-win-5.4.1 \
	cmake-0.1.58 \
	codespan-reporting-0.12.0 \
	combine-4.6.8 \
	concurrent-queue-2.5.0 \
	const-oid-0.10.2 \
	core-foundation-0.9.4 \
	core-foundation-0.10.1 \
	core-foundation-sys-0.8.7 \
	core-graphics-0.23.2 \
	core-graphics-0.24.0 \
	core-graphics-types-0.1.3 \
	core-graphics-types-0.2.0 \
	core-text-21.0.0 \
	cpufeatures-0.3.1 \
	crc32fast-1.5.2 \
	crossbeam-utils-0.8.23 \
	crossfont-0.9.0 \
	crunchy-0.2.4 \
	crypto-common-0.2.2 \
	cursor-icon-1.2.0 \
	curve25519-dalek-5.0.0 \
	curve25519-dalek-derive-0.1.1 \
	deranged-0.4.0 \
	digest-0.11.3 \
	dispatch-0.2.0 \
	dispatch2-0.3.1 \
	dlib-0.5.3 \
	document-features-0.2.12 \
	downcast-rs-1.2.1 \
	dpi-0.1.2 \
	dunce-1.0.5 \
	dwrote-0.11.5 \
	ed25519-3.0.0 \
	ed25519-dalek-3.0.0 \
	equivalent-1.0.2 \
	errno-0.3.14 \
	error-code-3.4.0 \
	fastrand-2.5.0 \
	fdeflate-0.3.7 \
	fhex-2.0.2 \
	fiat-crypto-0.3.0 \
	filedescriptor-0.8.3 \
	find-msvc-tools-0.1.14 \
	fixedbitset-0.5.7 \
	flate2-1.1.10 \
	fnv-1.0.7 \
	foldhash-0.1.5 \
	font-types-0.12.6 \
	fontconfig-parser-0.5.8 \
	fontdb-0.24.0 \
	foreign-types-0.5.0 \
	foreign-types-macros-0.2.4 \
	foreign-types-shared-0.3.1 \
	freetype-rs-0.36.0 \
	freetype-sys-0.20.1 \
	fs_extra-1.3.0 \
	futures-core-0.3.34 \
	futures-task-0.3.34 \
	futures-util-0.3.34 \
	gethostname-1.1.0 \
	getrandom-0.2.17 \
	getrandom-0.3.4 \
	getrandom-0.4.3 \
	gl_generator-0.14.0 \
	glow-0.16.0 \
	glutin_wgl_sys-0.6.1 \
	gpu-alloc-0.6.2 \
	gpu-alloc-types-0.3.1 \
	gpu-allocator-0.27.0 \
	gpu-descriptor-0.3.2 \
	gpu-descriptor-types-0.2.0 \
	half-2.7.1 \
	harfrust-0.14.0 \
	hashbrown-0.14.5 \
	hashbrown-0.15.5 \
	hashbrown-0.17.1 \
	hermit-abi-0.5.3 \
	hexf-parse-0.2.1 \
	hybrid-array-0.4.15 \
	iana-time-zone-0.1.65 \
	iana-time-zone-haiku-0.1.2 \
	image-0.25.9 \
	image-webp-0.2.4 \
	indexmap-2.14.2 \
	itoa-1.0.18 \
	jni-0.22.4 \
	jni-macros-0.22.4 \
	jni-sys-0.3.1 \
	jni-sys-0.4.1 \
	jni-sys-macros-0.4.1 \
	jobserver-0.1.35 \
	js-sys-0.3.106 \
	khronos-egl-6.0.0 \
	khronos_api-3.1.0 \
	lazy_static-1.5.0 \
	libc-0.2.190 \
	libloading-0.8.9 \
	libm-0.2.16 \
	libredox-0.1.25 \
	linux-raw-sys-0.4.15 \
	linux-raw-sys-0.12.1 \
	litrs-1.0.0 \
	lock_api-0.4.14 \
	log-0.4.34 \
	lsonar-0.2.4 \
	malloc_buf-0.0.6 \
	memchr-2.8.3 \
	memmap2-0.9.11 \
	memoffset-0.9.1 \
	metal-0.32.0 \
	miniz_oxide-0.8.9 \
	miniz_oxide-0.9.1 \
	moxcms-0.7.11 \
	naga-26.0.0 \
	ndk-0.9.0 \
	ndk-context-0.1.1 \
	ndk-sys-0.6.0+11769913 \
	nix-0.28.0 \
	nix-0.31.3 \
	nom-8.0.0 \
	num-conv-0.1.0 \
	num-traits-0.2.19 \
	num_enum-0.7.6 \
	num_enum_derive-0.7.6 \
	objc-0.2.7 \
	objc-sys-0.3.5 \
	objc2-0.5.2 \
	objc2-0.6.4 \
	objc2-app-kit-0.2.2 \
	objc2-app-kit-0.3.2 \
	objc2-cloud-kit-0.2.2 \
	objc2-contacts-0.2.2 \
	objc2-core-data-0.2.2 \
	objc2-core-foundation-0.3.2 \
	objc2-core-graphics-0.3.2 \
	objc2-core-image-0.2.2 \
	objc2-core-location-0.2.2 \
	objc2-encode-4.1.0 \
	objc2-foundation-0.2.2 \
	objc2-foundation-0.3.2 \
	objc2-io-surface-0.3.2 \
	objc2-link-presentation-0.2.2 \
	objc2-metal-0.2.2 \
	objc2-quartz-core-0.2.2 \
	objc2-symbols-0.2.2 \
	objc2-ui-kit-0.2.2 \
	objc2-uniform-type-identifiers-0.2.2 \
	objc2-user-notifications-0.2.2 \
	once_cell-1.21.4 \
	orbclient-0.3.55 \
	ordered-float-5.0.0 \
	os_pipe-1.2.3 \
	owned_ttf_parser-0.25.1 \
	parking_lot-0.12.5 \
	parking_lot_core-0.9.12 \
	paste-1.0.15 \
	percent-encoding-2.3.2 \
	petgraph-0.8.3 \
	pin-project-1.1.13 \
	pin-project-internal-1.1.13 \
	pin-project-lite-0.2.17 \
	pkg-config-0.3.34 \
	plain-0.2.3 \
	png-0.18.1 \
	polling-3.11.0 \
	pollster-1.0.1 \
	portable-atomic-1.15.0 \
	portable-atomic-util-0.2.8 \
	portable-pty-0.9.0 \
	powerfmt-0.2.1 \
	ppv-lite86-0.2.21 \
	presser-0.3.1 \
	proc-macro-crate-3.5.0 \
	proc-macro2-1.0.107 \
	profiling-1.0.18 \
	proptest-1.11.0 \
	pxfm-0.1.30 \
	quick-error-1.2.3 \
	quick-error-2.0.1 \
	quick-xml-0.41.0 \
	quote-1.0.47 \
	r-efi-5.3.0 \
	r-efi-6.0.0 \
	rand-0.9.5 \
	rand_chacha-0.9.0 \
	rand_core-0.9.5 \
	rand_xorshift-0.4.0 \
	range-alloc-0.1.5 \
	raw-window-handle-0.6.2 \
	rcgen-0.13.2 \
	read-fonts-0.41.0 \
	read-fonts-0.45.0 \
	redox_syscall-0.4.1 \
	redox_syscall-0.5.18 \
	redox_syscall-0.9.4 \
	regex-syntax-0.8.11 \
	renderdoc-sys-1.1.0 \
	ring-0.17.14 \
	roxmltree-0.20.0 \
	rustc-hash-1.1.0 \
	rustc_version-0.4.1 \
	rustix-0.38.44 \
	rustix-1.1.5 \
	rustls-0.23.46 \
	rustls-pki-types-1.15.1 \
	rustls-webpki-0.103.16 \
	rustversion-1.0.23 \
	rusty-fork-0.3.1 \
	same-file-1.0.6 \
	scoped-tls-1.0.1 \
	scopeguard-1.2.0 \
	sctk-adwaita-0.10.1 \
	semver-1.0.28 \
	serde-1.0.229 \
	serde_core-1.0.229 \
	serde_derive-1.0.229 \
	serde_json-1.0.151 \
	serial2-0.2.38 \
	sha2-0.11.0 \
	shared_library-0.1.9 \
	shell-words-1.1.1 \
	shlex-2.0.1 \
	signature-3.0.0 \
	simd-adler32-0.3.10 \
	simd_cesu8-1.2.0 \
	simdutf8-0.1.5 \
	skrifa-0.44.0 \
	slab-0.4.12 \
	slotmap-1.1.1 \
	smallvec-1.16.2 \
	smithay-client-toolkit-0.19.2 \
	smol_str-0.2.2 \
	spirv-0.3.0+sdk-1.3.268.0 \
	sptr-0.3.2 \
	stable_deref_trait-1.2.1 \
	static_assertions-1.1.0 \
	strict-num-0.1.1 \
	subtle-2.6.1 \
	swash-0.2.10 \
	syn-2.0.119 \
	syn-3.0.6 \
	synstructure-0.13.2 \
	synstructure-0.14.0 \
	tempfile-3.27.0 \
	termcolor-1.4.1 \
	thiserror-1.0.69 \
	thiserror-2.0.21 \
	thiserror-impl-1.0.69 \
	thiserror-impl-2.0.21 \
	time-0.3.41 \
	time-core-0.1.4 \
	tiny-skia-0.11.4 \
	tiny-skia-path-0.11.4 \
	tinyvec-1.13.3 \
	tokio-1.53.1 \
	toml_datetime-1.1.1+spec-1.1.0 \
	toml_edit-0.25.15+spec-1.1.0 \
	toml_parser-1.1.3+spec-1.1.0 \
	tracing-0.1.44 \
	tracing-core-0.1.36 \
	tree_magic_mini-3.2.2 \
	ttf-parser-0.25.1 \
	typenum-1.20.1 \
	unarray-0.1.4 \
	unicode-ident-1.0.26 \
	unicode-segmentation-1.13.3 \
	unicode-width-0.2.2 \
	untrusted-0.9.0 \
	version_check-0.9.5 \
	vte-0.15.0 \
	wait-timeout-0.2.1 \
	walkdir-2.5.0 \
	wasi-0.11.1+wasi-snapshot-preview1 \
	wasip2-1.0.1+wasi-0.2.4 \
	wasm-bindgen-0.2.129 \
	wasm-bindgen-futures-0.4.79 \
	wasm-bindgen-macro-0.2.129 \
	wasm-bindgen-macro-support-0.2.129 \
	wasm-bindgen-shared-0.2.129 \
	wayland-backend-0.3.17 \
	wayland-client-0.31.15 \
	wayland-csd-frame-0.3.0 \
	wayland-cursor-0.31.14 \
	wayland-protocols-0.32.12 \
	wayland-protocols-plasma-0.3.12 \
	wayland-protocols-wlr-0.3.12 \
	wayland-scanner-0.31.11 \
	wayland-sys-0.31.11 \
	web-sys-0.3.106 \
	web-time-1.1.0 \
	wgpu-26.0.1 \
	wgpu-core-26.0.1 \
	wgpu-core-deps-apple-26.0.0 \
	wgpu-core-deps-emscripten-26.0.0 \
	wgpu-core-deps-windows-linux-android-26.0.0 \
	wgpu-hal-26.0.6 \
	wgpu-types-26.0.0 \
	winapi-0.3.9 \
	winapi-i686-pc-windows-gnu-0.4.0 \
	winapi-util-0.1.11 \
	winapi-x86_64-pc-windows-gnu-0.4.0 \
	windows-0.58.0 \
	windows-core-0.58.0 \
	windows-implement-0.58.0 \
	windows-interface-0.58.0 \
	windows-link-0.2.1 \
	windows-result-0.2.0 \
	windows-strings-0.1.0 \
	windows-sys-0.52.0 \
	windows-sys-0.59.0 \
	windows-sys-0.60.2 \
	windows-sys-0.61.2 \
	windows-targets-0.52.6 \
	windows-targets-0.53.5 \
	windows_aarch64_gnullvm-0.52.6 \
	windows_aarch64_gnullvm-0.53.1 \
	windows_aarch64_msvc-0.52.6 \
	windows_aarch64_msvc-0.53.1 \
	windows_i686_gnu-0.52.6 \
	windows_i686_gnu-0.53.1 \
	windows_i686_gnullvm-0.52.6 \
	windows_i686_gnullvm-0.53.1 \
	windows_i686_msvc-0.52.6 \
	windows_i686_msvc-0.53.1 \
	windows_x86_64_gnu-0.52.6 \
	windows_x86_64_gnu-0.53.1 \
	windows_x86_64_gnullvm-0.52.6 \
	windows_x86_64_gnullvm-0.53.1 \
	windows_x86_64_msvc-0.52.6 \
	windows_x86_64_msvc-0.53.1 \
	winit-0.30.13 \
	winnow-1.0.4 \
	winreg-0.10.1 \
	wio-0.2.2 \
	wit-bindgen-0.46.0 \
	wl-clipboard-rs-0.9.3 \
	x11-dl-2.21.0 \
	x11rb-0.13.2 \
	x11rb-protocol-0.13.2 \
	xcursor-0.3.11 \
	xkbcommon-dl-0.4.2 \
	xkeysym-0.2.1 \
	xml-rs-0.8.29 \
	yasna-0.5.2 \
	yazi-0.2.1 \
	yeslogic-fontconfig-sys-6.0.1 \
	yoke-0.8.3 \
	yoke-derive-0.8.4 \
	zeno-0.3.3 \
	zerocopy-0.8.59 \
	zerocopy-derive-0.8.59 \
	zerofrom-0.1.8 \
	zerofrom-derive-0.1.8 \
	zeroize-1.9.0 \
	zlib-rs-0.6.8 \
	zmij-1.0.23 \
	zune-core-0.5.3 \
	zune-jpeg-0.5.15 \
	bitty-a11y@git+https://github.com/bitty-terminal/bitty-a11y?rev=8e02ab5\#8e02ab5fe3cf1984fe181c91f08c3e3d81292fda \
	bitty-a11y@git+https://github.com/bitty-terminal/bitty-a11y?rev=99486eef216d77b8f81d2b67f2cef856af266a3f\#99486eef216d77b8f81d2b67f2cef856af266a3f \
	bitty-execution@git+https://github.com/bitty-terminal/bitty-execution?rev=36404ab\#36404ab9302a3119b404d3bd1cca44fa7230c3bc \
	bitty-ipc,bitty-ipc-api,bitty-ipc-auth,bitty-ipc-core,bitty-ipc-devtools,bitty-ipc-winpipe@git+https://github.com/bitty-terminal/bitty-ipc?rev=9493674\#949367413b0640fbea4b99c6dda9acff48c623d1 \
	bitty-network-publication,bitty-network-wire@git+https://github.com/bitty-terminal/bitty-network?rev=c8710e0\#c8710e0c6f86a44a91398c121123d5b7e43ba3c1 \
	bitty-observability-api@git+https://github.com/bitty-terminal/bitty-observability?rev=f1d08f4\#f1d08f4f3c2e95cd52e91796a73fb443657fa46d \
	bitty-platform-services@git+https://github.com/bitty-terminal/bitty-platform-services?rev=7c6b84b\#7c6b84b795b02afde5f4507f3f5a25d8474bf8fe \
	bitty-storage@git+https://github.com/bitty-terminal/bitty-storage?rev=7bf5177\#7bf5177c668fb72ec683c7cb8270049f6400f204 \
	bitty-url-detector@git+https://github.com/bitty-terminal/bitty-url-detector?rev=be035a0\#be035a0612617ee88fbfbc24ff4c2fb079f9cd7b \
	phodopus,phodopus-gc-arena,phodopus-gc-arena-derive@git+https://github.com/bitty-terminal/phodopus?rev=91b8a2c\#91b8a2ceb77994aeaf3f8485862a400671bbed5e

.include <bsd.port.mk>
