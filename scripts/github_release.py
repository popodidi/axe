#!/usr/bin/env python3
"""Create a GitHub Release for a tag and upload its assets (stdlib only).

Used by .github/workflows/release.yml with the workflow's GITHUB_TOKEN. The
release is created as a draft, assets are uploaded, then it is published, so a
failure never leaves a published release with missing assets. An existing
release for the tag is an error: release tags are never reused.

REST API: https://docs.github.com/en/rest/releases/releases and
https://docs.github.com/en/rest/releases/assets#upload-a-release-asset
"""
import argparse
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

API = os.environ.get("GITHUB_API_URL", "https://api.github.com")
UPLOADS = "https://uploads.github.com"


def call(method, url, token, body=None, data=None, content_type="application/json"):
    if body is not None:
        data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("X-GitHub-Api-Version", "2022-11-28")
    if data is not None:
        req.add_header("Content-Type", content_type)
    try:
        with urllib.request.urlopen(req, timeout=300) as resp:
            raw = resp.read()
            return resp.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--repo", required=True)
    p.add_argument("--tag", required=True)
    p.add_argument("--notes-file", required=True)
    p.add_argument("--prerelease", action="store_true")
    p.add_argument("assets", nargs="+")
    a = p.parse_args()
    token = os.environ["GITHUB_TOKEN"]
    base = f"{API}/repos/{a.repo}/releases"

    status, existing = call("GET", f"{base}/tags/{urllib.parse.quote(a.tag)}", token)
    if status != 404:
        sys.exit(f"release for {a.tag} already exists or lookup failed (HTTP {status}); tags are never reused")

    status, rel = call("POST", base, token, body={
        "tag_name": a.tag,
        "name": a.tag,
        "body": Path(a.notes_file).read_text(),
        "draft": True,
        "prerelease": a.prerelease,
    })
    if status != 201:
        sys.exit(f"create release failed: HTTP {status}: {rel}")
    print(f"draft release {rel['id']} created")

    for path in map(Path, a.assets):
        url = f"{UPLOADS}/repos/{a.repo}/releases/{rel['id']}/assets?name={urllib.parse.quote(path.name)}"
        status, asset = call("POST", url, token, data=path.read_bytes(),
                             content_type="application/octet-stream")
        if status != 201:
            sys.exit(f"upload {path.name} failed: HTTP {status}: {asset}")
        print(f"uploaded {path.name} ({asset['size']} bytes)")

    status, rel = call("PATCH", f"{base}/{rel['id']}", token, body={"draft": False})
    if status != 200:
        sys.exit(f"publish release failed: HTTP {status}: {rel}")
    print(f"published {rel['html_url']}")


if __name__ == "__main__":
    main()
