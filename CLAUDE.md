# photos

Reusable storage domain for any Eco estate. It stores images, documents, and
videos in S3-compatible MinIO and hands consumers opaque object keys, never S3
details. Images are converted to WebP when that is smaller; PDF, Word, Excel,
and text documents are stored as-is; videos are re-encoded to MP4 (H.264/AAC)
with `ffmpeg`.

## Structure

```
photos/
└── backend/     # Rust backend service (Axum + aws-sdk-s3)
```

## Upload limits are env-driven

Never hardcode size caps in this repo. The backend reads them from the
service `.env` (defaults live in `backend/.env.example`):

- `MAX_IMAGE_MB` (default `10`) — image size cap
- `MAX_DOCUMENT_MB` (default `50`) — document size cap
- `MAX_IMAGE_PIXELS` (default `40000000`) — image pixel-count cap (width × height)
- `MAX_VIDEO_MB` (default `200`) — video size cap (checked on the *uploaded*
  file, before ffmpeg compresses it)

They are loaded into `UploadLimits` on `AppState` in `backend/src/main.rs`
and drive both the tower_http/axum body limits and the `process_upload` /
`transcode_video` rejection messages (which render the configured MB value,
not a literal). Body limits use the **largest** kind via `max_body_bytes()`.

## Videos

### Compression (upload)

Uploads with a video MIME type (`is_video()`) skip the in-memory image/doc
path. `upload_object` streams the multipart field to a temp file (never
buffers it in RAM), then `transcode_video()` runs `ffmpeg`:

```
ffmpeg -y -i <input> -c:v libx264 -preset fast -crf 26 -maxrate 2M -bufsize 4M \
  -vf "scale='min(1280,iw)':-2" -pix_fmt yuv420p -r 30 -c:a aac -b:a 128k \
  -movflags +faststart -f mp4 <output>
```

Output is always `video/mp4`, stored with `kind: "video"`, original uploaded
extension replaced by `.mp4`. **Silent clips** (no audio stream) fail the
audio-mapped encode; the code retries once with `-an`. Transcoding is
synchronous — fine for short product carousel clips (~seconds on the estate),
but NOT for long course videos: those will need an async job + status
endpoint before going live.

### Delivery (streaming)

`GET /api/storage/content/*key` is Range-aware: single `Range: bytes=…`
headers get `206 Partial Content` + `Content-Range` + `Accept-Ranges: bytes`,
streamed from S3 via `into_async_read()` + `ReaderStream` (never collected
into RAM like the old code did). `<video src>` therefore starts instantly and
seeks. HEAD is served by `download_headers()` for probing. Suffix/multi-range
headers fall back to a plain `200` full object. Videos are served
`Content-Disposition: inline`.

### Course videos (future, designed not built)

Same Range endpoint, but protected: sign `key + expiry` with a shared HMAC
secret and stream only when the `?token=` validates. The frontend (course
domain) checks enrollment before issuing the token. The photos API has no
auth today — do not expose course URLs unauthenticated.

### Gotchas

- **ffmpeg is a runtime dependency, not a Cargo dep.** If it's missing the
  video upload returns `ffmpeg tidak ditemukan` (the `Command::new` error
  path). Add `ffmpeg` to the estate `ecompose.yml` `shared_tools` so Eco
  installs it. CT 101 (10 cores / 4GB RAM) is fine for short carousel clips,
  tight for long course encodes — see the async-job note above.
- **Cloudflare edge caps request bodies ~100MB on the free plan.** A
  `MAX_VIDEO_MB` above that means very large uploads fail at Cloudflare before
  reaching this backend. Carousel clips are far below this; >100MB course
  uploads need presigned direct-to-MinIO or Cloudflare Stream (both designed
  for later, not built).
- The body-limit layers must always cover the largest kind or oversize videos
  get a generic multipart 400 instead of "Ukuran video maksimal 200 MB".

## Gotcha: the frontend enforces its own limit too (2026-08-07)

Uploads are gated *client-side* by the consuming frontend before they ever
reach this backend. On the stuff8 estate that check lives in
`stuff8_composition/frontend/src/pages/inventory/form/index.astro`, driven by
`PUBLIC_MAX_PHOTO_MB` (default `10`) — declared in that repo's
`frontend/.env.example` so Eco propagates it.

The failure that led to env-driven limits on both sides: the backend was
bumped to 10MB and the webhook had rebuilt and restarted it (verified via the
deploy webhook log and `strings` on the running binary), yet >5MB uploads
still failed in production. The blocker was a hardcoded `5 * 1024 * 1024`
cap in the frontend script, which rejected the file in the browser and never
sent it. Backend-only changes are invisible to the user until the frontend's
`PUBLIC_MAX_PHOTO_MB` matches.

So: changing an upload cap means touching **both** sides and redeploying both
repos — bump the backend `MAX_IMAGE_MB`/`MAX_DOCUMENT_MB` *and* the frontend
`PUBLIC_MAX_PHOTO_MB`, then verify live (e.g. a >5MB image returns 415
"File gambar tidak valid" rather than 413 "Ukuran gambar maksimal 5 MB").

Videos have the same pairing: backend `MAX_VIDEO_MB` ↔ frontend
`PUBLIC_MAX_VIDEO_MB` (default `200`), enforced in
`inventory/form/index.astro` via `MAX_VIDEO_BYTES` on `video/*` files. The
client-side cap applies to the *uploaded* file size, before ffmpeg compresses
it.

## API

Base path `/api/storage`. Consumers on the stuff8 estate reach it through the
estate gateway as `/api/...` or the `photos.stuff8.com` hostname
(`expose.additional` in `ecompose.yml`).

- `POST /api/storage/objects` — multipart `file`, `owner_id`, `namespace`,
  `reference_id` → stores and returns object metadata. Images → downscaled to
  `IMAGE_MAX_DIMENSION` (default 1600) WebP when smaller, plus an auto-generated
  `-thumb.webp` sibling (`thumbnail_key` in the response, sized by
  `THUMBNAIL_DIMENSION`, default 400); documents stored as-is; videos → ffmpeg
  MP4 (`kind: "video"`)
- `GET /api/storage/objects?owner_id=&namespace=&reference_id=` — list owned
  objects
- `GET /api/storage/content/*key` — retrieve content (Range streaming; images
  and videos served inline so browsers render/play them in place). List/grid
  views should load the `thumbnail_key` URL instead of the full image.
- `GET /api/storage/objects/*key` — object metadata
- `DELETE /api/storage/objects/*key?owner_id=` — remove an owned object (also
  removes its `-thumb.webp` sibling)

`owner_id`, `namespace`, `reference_id`, and path segments are validated as
`[A-Za-z0-9._-]` (`safe_segment`); S3 keys stay opaque to consumers.

## Thumbnails (2026-08-07)

Every image upload now produces two objects: the full WebP (downscaled to at
most `IMAGE_MAX_DIMENSION` px on the long edge, so a 12MP phone photo is no
longer a multi-MB download) and a small WebP thumbnail at `<uuid>-thumb.webp`
(`THUMBNAIL_DIMENSION` px). `POST /api/storage/objects` returns the thumbnail
key as `thumbnail_key`; the frontend serves it from list/grid views (`/inventory`,
`/marketplace`) and the full image from detail pages, lightboxes and OG images.

Pre-existing images uploaded before this change have **no** thumbnail object —
a `-thumb.webp` request 404s. The consuming frontend must fall back to the full
image (onerror swap to `.../storage/content/{key}`) so old inventory items keep
rendering.

## ffmpeg hang protection (2026-08-07)

`run_ffmpeg` now enforces a 120s timeout and kills the child on timeout, so a
stalled/corrupt video returns a clear `408 Request Timeout` instead of holding
the upload request (and the Cloudflare tunnel) open forever. `transcode_video`
also removes its temp output on failure. The *input* temp file is removed in
`upload_object` as before — but only after transcode returns; a timeout now
returns before that, so orphaned `eco-video-in-*` files are gone too.
