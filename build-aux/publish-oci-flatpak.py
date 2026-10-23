#!/usr/bin/env python3
"""Publish flatpak build-bundle --oci output to a Forgejo OCI registry."""

from __future__ import annotations

import base64
import hashlib
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


def fail(message: str) -> None:
    raise SystemExit(message)


def request(method: str, url: str, data: bytes | None = None, content_type: str | None = None):
    headers = {"Authorization": AUTH_HEADER}
    if content_type:
        headers["Content-Type"] = content_type
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        return urllib.request.urlopen(req, timeout=180)
    except urllib.error.HTTPError as error:
        if method == "HEAD" and error.code == 404:
            return error
        detail = error.read(1024).decode("utf-8", errors="replace")
        fail(f"{method} {url} failed: HTTP {error.code} {detail}")


def main() -> None:
    if len(sys.argv) < 3:
        fail(f"usage: {Path(sys.argv[0]).name} ARCH OCI-LAYOUT [TAG...]")
    arch, layout_arg, *tags = sys.argv[1:]
    if not tags:
        tags = ["stable", "oci-test"]
    layout = Path(layout_arg)
    if not (layout / "index.json").is_file() or not (layout / "oci-layout").is_file():
        fail(f"not an OCI image layout: {layout}")

    token = os.environ.get("PACKAGES_TOKEN", "")
    if not token:
        print("Skipping OCI publish: add the PACKAGES_TOKEN secret")
        return

    owner = os.environ.get("PACKAGES_OWNER", "toxblh")
    server = os.environ.get("FORGEJO_SERVER_URL", "https://altlinux.space").rstrip("/")
    base = f"{server}/v2"
    repository = f"{owner}/ru.toxblh.matras-{arch}-stable"
    ref = f"app/ru.toxblh.Matras/{arch}/stable"
    index = json.loads((layout / "index.json").read_text())
    descriptor = next(
        (
            item
            for item in index.get("manifests", [])
            if item.get("annotations", {}).get("org.opencontainers.image.ref.name") == ref
        ),
        None,
    )
    if descriptor is None:
        fail(f"OCI layout has no Flatpak ref {ref}")

    config_path = layout / "blobs" / "sha256" / descriptor["digest"].removeprefix("sha256:")
    manifest = json.loads(config_path.read_bytes())
    config_descriptor = manifest["config"]
    config_path = layout / "blobs" / "sha256" / config_descriptor["digest"].removeprefix("sha256:")
    config = json.loads(config_path.read_bytes())
    labels = config.get("config", {}).get("Labels", {})
    if labels.get("org.flatpak.ref") != ref:
        fail(f"OCI image labels do not match {ref}")

    password = os.environ["PACKAGES_TOKEN"]
    credentials = base64.b64encode(f"{owner}:{password}".encode()).decode()
    global AUTH_HEADER
    AUTH_HEADER = f"Basic {credentials}"

    for blob in [manifest["config"], *manifest.get("layers", [])]:
        digest = blob["digest"]
        blob_url = f"{base}/{repository}/blobs/{digest}"
        with request("HEAD", blob_url) as response:
            if response.status == 200:
                continue
        local_blob = layout / "blobs" / "sha256" / digest.removeprefix("sha256:")
        payload = local_blob.read_bytes()
        actual = "sha256:" + hashlib.sha256(payload).hexdigest()
        if actual != digest:
            fail(f"OCI blob checksum mismatch: {local_blob}")
        with request("POST", f"{base}/{repository}/blobs/uploads/") as response:
            location = response.headers.get("Location")
            if not location:
                fail("Registry did not return a blob upload location")
        upload_url = urllib.parse.urljoin(f"{server}/", location)
        separator = "&" if "?" in upload_url else "?"
        with request(
            "PUT",
            f"{upload_url}{separator}digest={urllib.parse.quote(digest, safe=':')}",
            payload,
            "application/octet-stream",
        ) as response:
            if response.status not in (201, 202):
                fail(f"Registry blob upload returned HTTP {response.status}")

    manifest_descriptor = descriptor["digest"]
    manifest_path = layout / "blobs" / "sha256" / manifest_descriptor.removeprefix("sha256:")
    manifest_bytes = manifest_path.read_bytes()
    media_type = descriptor.get("mediaType", "application/vnd.oci.image.manifest.v1+json")
    for tag in tags:
        with request(
            "PUT",
            f"{base}/{repository}/manifests/{urllib.parse.quote(tag, safe='._-')}",
            manifest_bytes,
            media_type,
        ) as response:
            if response.status not in (200, 201, 202):
                fail(f"Registry manifest publish returned HTTP {response.status}")
        print(f"Published Flatpak OCI {repository}:{tag} (commit {labels['org.flatpak.commit']})")


if __name__ == "__main__":
    AUTH_HEADER = ""
    main()
