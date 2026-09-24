# Issue #18 — Forgejo Flatpak OCI registry support

## Verification result

The current ALS Forgejo instance does not expose the Flatpak OCI registry
index endpoint. Flatpak OCI remotes request `/index/static`; anonymous GETs to
both `https://altlinux.space/index/static` and
`https://altlinux.space/toxblh/matras-flatpak/index/static` return HTTP 404
(`text/plain`, `Not found.`).

Forgejo's container registry can store OCI images, but the missing index
endpoint is required for Flatpak clients to discover app references. The
upstream Forgejo change adding Flatpak package support is still an open pull
request: <https://codeberg.org/forgejo/forgejo/pulls/10233> (checked
2026-09-24). The instance reports Forgejo `16.0.2-17` via the ALS Actions skill
operational notes.

## Acceptance status

Issue #18 remains open. Uploading `flatpak build-bundle --oci` output to the
Container registry alone would create an OCI package entry but would not make
an installable/updateable Flatpak remote on this server. Replacing the current
Generic bundle before `/index/static` is available would therefore leave the
reported delivery problem unresolved.
