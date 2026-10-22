Name:           bitty
Version:        0.0.26
Release:        1%{?dist}
Summary:        Bitty pre-alpha terminal workspace minimal correct terminal
License:        MIT OR Apache-2.0
URL:            https://github.com/bitty-terminal/bitty
Source0:        https://cdn.bitty.run/releases/v%{version}/bitty-%{version}-x86_64-unknown-linux-gnu.tar.zst
Source1:        https://cdn.bitty.run/releases/v%{version}/bitty-%{version}-aarch64-unknown-linux-gnu.tar.zst

Provides:       bitty-terminal = %{version}-%{release}
Requires:       fontconfig
Requires:       freetype
Requires:       libgcc
BuildRequires:  ncurses

%description
A programmable, microkernel terminal emulator built around a small Rust core
and a two-tiered extension architecture.

%prep
%ifarch x86_64
%setup -q -c -T -a 0
%endif
%ifarch aarch64
%setup -q -c -T -a 1
%endif

%build
# Precompiled release bundle; compilation is performed during release CI.

%install
rm -rf %{buildroot}
%ifarch x86_64
cd bitty-%{version}-x86_64-unknown-linux-gnu
%endif
%ifarch aarch64
cd bitty-%{version}-aarch64-unknown-linux-gnu
%endif

install -d %{buildroot}%{_bindir}
install -d %{buildroot}%{_datadir}/applications
install -d %{buildroot}%{_datadir}/metainfo
install -d %{buildroot}%{_datadir}/terminfo
install -d %{buildroot}%{_datadir}/licenses/%{name}
install -d %{buildroot}%{_docdir}/%{name}

install -m 0755 bin/bitty %{buildroot}%{_bindir}/bitty
install -m 0644 share/applications/run.bitty.Bitty.desktop %{buildroot}%{_datadir}/applications/run.bitty.Bitty.desktop
install -m 0644 share/metainfo/run.bitty.Bitty.metainfo.xml %{buildroot}%{_datadir}/metainfo/run.bitty.Bitty.metainfo.xml
install -m 0644 LICENSE %{buildroot}%{_datadir}/licenses/%{name}/LICENSE
install -m 0644 README.md %{buildroot}%{_docdir}/%{name}/README.md
install -m 0644 CHANGELOG.md %{buildroot}%{_docdir}/%{name}/CHANGELOG.md

for size in 16 32 64 128 256 512; do
    install -d %{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps
    install -m 0644 share/icons/hicolor/${size}x${size}/apps/bitty.png \
        %{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps/bitty.png
done
install -d %{buildroot}%{_datadir}/icons/hicolor/scalable/apps
install -m 0644 share/icons/hicolor/scalable/apps/bitty.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/bitty.svg

tic -x -o %{buildroot}%{_datadir}/terminfo terminfo/bitty.ti

%files
%license LICENSE
%doc README.md CHANGELOG.md
%{_bindir}/bitty
%{_datadir}/applications/run.bitty.Bitty.desktop
%{_datadir}/metainfo/run.bitty.Bitty.metainfo.xml
%{_datadir}/icons/hicolor/*/apps/bitty.*
%{_datadir}/terminfo/b/bitty

%changelog
* Thu Oct 22 2026 Bitty Maintainers <maintainers@bitty-terminal.dev> - 0.0.26-1
- Release 0.0.26
