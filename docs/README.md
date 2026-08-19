# storage — LXS docs

## Capability

Photos/media object store for any Eco estate. Consumes multipart uploads of
images, documents, and videos; hands back an opaque object key (never S3
details). Images are downscaled and re-encoded to WebP (when smaller) plus an
auto-generated `-thumb.webp` thumbnail; PDF/Word/Excel/text documents pass
through as-is; videos are transcoded to MP4 (H.264/AAC) via `ffmpeg` and
served with Range-aware streaming for instant `<video>` playback and seeking.
If you need upload→process→serve media with thumbnails, this is the LXS.

## What it owns / never owns

- **Owns:** the S3/MinIO bucket and every object in it (`{namespace}/
  {reference_id}/{uuid}.{ext}` keys), object metadata (S3 user-metadata:
  `owner-id`, `namespace`, `reference-id`, `original-name`, `kind`,
  `created-at`), image downscale/WebP conversion, `-thumb.webp` thumbnail
  generation, video transcoding, and Range-aware content streaming.
- **Never owns:** deck/course documents or their business state (that is
  `slides`/`courses`); user identity or entitlements — the API has **no auth**
  today and must not be exposed for protected content. MinIO provisioning is
  managed by Eco (`storage.minio`), not by this backend (though it auto-creates
  its bucket at startup).

## Compose it

```yaml
# ecompose.yml
services:
  storage-backend:
    lxs: storage@1.0.5
    grants:
      secrets: [SERVER_PORT, API_BASE_URL, S3_ENDPOINT, S3_REGION, S3_BUCKET, S3_ACCESS_KEY, S3_SECRET_KEY]
    shared_tools: [ffmpeg]   # required for video uploads
```

Requires an S3-compatible endpoint (MinIO via `eco install minio`) — all of
`S3_ENDPOINT`, `S3_ACCESS_KEY`, `S3_SECRET_KEY` are required at startup
(`S3_BUCKET` defaults to `eco-storage`, `S3_REGION` to `us-east-1`).

## Quick usage

```sh
# Upload an image (multipart: file + owner_id + namespace + reference_id)
curl -F "file=@photo.jpg;type=image/jpeg" \
     -F "owner_id=u_123" -F "namespace=inventory" -F "reference_id=item-42" \
     http://127.0.0.1:8081/api/storage/objects
# → 201 { "key": "inventory/item-42/<uuid>.webp", "thumbnail_key": "inventory/item-42/<uuid>-thumb.webp", "kind": "image", ... }

# Fetch the thumbnail for list/grid views
curl -s -o thumb.webp "http://127.0.0.1:8081/api/storage/content/inventory/item-42/<uuid>-thumb.webp"
```

## Docs index

- `api.md` — full endpoint reference with request/response JSON and errors
- `examples.sh` — executable smoke test (golden request→response pairs)
- `openapi.json` — machine-readable OpenAPI 3.0 spec
- `changelog.md` — version history + breaking changes
- `gotchas.md` — production-learned constraints and operational gotchas

## For AI agents

This LXS is distributed as a **binary only** — these docs are the entire
interface. Match `api.md` shapes exactly; run `examples.sh` against a pulled
binary or live estate URL before trusting behavior. See
`docs/gotchas.md` for constraints that are invisible in the binary (ffmpeg
must be installed, no auth, Cloudflare ~100MB edge cap, the frontend's own
upload caps, 300s ffmpeg timeout, pre-existing objects without thumbnails).
