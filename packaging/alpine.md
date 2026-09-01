# Alpine

Alpine uses apk. The apk produced by `nfpm package --packager apk` is the Alpine artifact.

Validation on Alpine container:

```sh
apk add --allow-untrusted ./dist/bitty-*.apk
apk info -L bitty
```

Alpine 3.20+ tested; no scripts with unbounded work, `apk` packager uses `apk` scripts no-ops.
