# OpenSUSE

OpenSUSE uses rpm. The rpm produced by `nfpm package --packager rpm` is the OpenSUSE artifact.

Validation on OpenSUSE container:

```sh
zypper install --allow-unsigned-rpm ./dist/bitty-*.rpm
rpm -qip ./dist/bitty-*.rpm
rpm -ql bitty
```

The same rpm is uploaded to the GitHub Release and installs via `zypper` on Tumbleweed and Leap. No release `verify-install` leg covers OpenSUSE, so it stays best-effort, not supported: a CI-built rpm is not support evidence (see `packaging/README.md` `Support evidence`).
