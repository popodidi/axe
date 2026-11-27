---
title: "Blobs"
description: "Immutable files on the server, named by their sha256."
---

Paths in `crates/axe/src/` unless noted.

`axe blob put/get/ls/link` (`blob.rs`) keeps immutable files on the server,
named by the sha256 of their bytes, so the same bytes are stored once. There
is no delete, overwrite or version. Contents live on local disk next to the
database, or in an S3-compatible bucket with `[blobs.s3]`. `axe blob link`
signs a URL (`/b/<sha256>/<name>?exp=..&sig=..`, at most seven days) that
opens without the token, for example from a phone; it renders inline in a
sandbox, and answers HEAD and single byte ranges (`206`, `416` past the
end) so video plays in Safari. The signing key is created on first start; `[blobs]` sets the
directory, size limit, link base URL and key file. `[blobs] link_url` moves
links to a separate site (a host that is not a subdomain of the server's):
there blob HTML may run scripts in an opaque-origin sandbox and load
scripts, styles, fonts and images over https and audio or video from the
blob site itself (`media-src 'self'`), but cannot fetch, connect or
post (`connect-src 'none'`, `form-action 'none'`); the host
answers only `/b/` links and never a session, and `/b/` links on the
server's own host redirect to it.

The dashboard's Store view lists and searches blobs and docs; a selected
item shows its metadata and an "Open full" link (the signed `/b/` URL, or
`/doc?path=` for a doc) beside a preview. A task's
artifacts preview store links inline (images, text, Markdown, sandboxed
HTML), re-signing an expired link from its sha256. Older artifact URLs open
too when a link-map doc (any doc path ending in `.links.tsv`, with `url` and
`sha256` columns; `blob_refs.rs`) maps them to a blob.
