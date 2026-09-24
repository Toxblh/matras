# Issue #18 — Forgejo Flatpak OCI registry support

## Result

Forgejo's Container registry serves the Flatpak OCI index at
`/api/packages/toxblh/container/index/static`. Anonymous requests returned
HTTP 200 for both x86_64 (`architecture=amd64`) and aarch64 (`architecture=arm64`),
with `org.flatpak.ref` labels for `app/ru.toxblh.Matras/<arch>/stable`.

The OCI publisher was exercised by Forgejo Actions run
[83951](https://altlinux.space/toxblh/matras/actions/runs/68). Both the x86_64
and aarch64 jobs succeeded and published `stable` and `oci-test` image tags.
The x86_64 image manifest digest was
`sha256:12a262ddb094241e9da2591558c9c16737a27919227592dac672fd0956a3b69c`
with Flatpak commit label
`135fbf38149b9301b44e448ab0a02d7cdabc25734c776c2d0e65e282f1b99ff1`. The
aarch64 image manifest digest was
`sha256:140d4de3700c9bb6918b43f28eef99fe4ab33c15e2af66a7d92de3f5a59d467a`
with Flatpak commit label
`25798d0a42251c7d4fa7dfa27e042f14f15bcdb7d8380433658759aa3a635bea`.

## Install and update test

Installed the `oci-test` image in a separate user installation. Initial
installed commit: `53f3da8d6ef39bfb379091f2ad879adb37858e86a4f633850789cfa94b101036`.
After publishing an updated image and refreshing the OCI remote index,
`flatpak update` installed commit `16f754ce06a7c97a23132e81ed56225f525facc1344994253d180aff47ce92ac`.
The updated app launched from that user installation.

Then changed that test remote to the production `stable` tag. It discovered
registry digest `sha256:12a262ddb094241e9da2591558c9c16737a27919227592dac672fd0956a3b69c`
and updated the installation again to commit
`c6356774663891f3d22d6fecb4ecba036758b8b8b2d2e974c81b1157e55a3935`.
This verifies installation and update from the published OCI image.

Flatpak cached the previous OCI index on the first update check. Re-adding the
remote refreshed that cached index; subsequent update from `stable` succeeded.

## Acceptance status

Issue #18 is fixed. OCI images are now the Flatpak publication path for both
architectures. The separate user-scope test left the existing system Flatpak
installation untouched.
