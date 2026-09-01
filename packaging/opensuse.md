# OpenSUSE

OpenSUSE uses rpm. The rpm produced by `nfpm package --packager rpm` is the OpenSUSE artifact.

Validation on OpenSUSE container:

```sh
zypper install --allow-unsigned-rpm ./dist/bitty-*.rpm
rpm -qip ./dist/bitty-*.rpm
rpm -ql bitty
```

The same rpm is uploaded to the GitHub Release and installed via `zypper` on Tumbleweed and Leap.
