# Gotchas

Production constraints that are NOT visible in the binary — from code comments
and `README.md`.

- **`ffmpeg` is a runtime dependency, not a Cargo dep.** Video uploads run
  `ffmpeg` via `Command::new("ffmpeg")`. If it's missing, the upload fails
  with a 400 `Invalid upload: ...` (the spawn error), NOT a clear message. Add
  `ffmpeg` to the estate's `ecompose.yml` `shared_tools` so Eco installs it.
  A 10-core/4GB CT-101 is fine for short carousel clips but tight for long
  encodes.
- **No auth anywhere.** Every endpoint is open. Do not expose protected
  content (e.g. course videos) through this API unauthenticated. The planned
  HMAC-signed `?token=` course-video protection is designed but **not built**.
- **The consuming frontend enforces its own upload caps (2026-08-07 lesson).**
  On the stuff8 estate the browser gates uploads before they ever reach this
  backend via `PUBLIC_MAX_PHOTO_MB` (default 10) and `PUBLIC_MAX_VIDEO_MB`
  (default 200) in the consuming frontend repo. A backend-only cap bump is
  invisible to users until the frontend's env matches — a real production
  incident (backend at 10MB, frontend still hardcoded to 5MB). Changing an
  upload cap means touching **both** sides and redeploying both.
- **Cloudflare edge caps request bodies ~100MB on the free plan.** If
  `MAX_VIDEO_MB` is raised above that, large uploads fail at Cloudflare before
  reaching this backend. Carousel clips are far below this; >100MB uploads
  need presigned direct-to-MinIO or Cloudflare Stream (both designed for
  later, not built).
- **ffmpeg timeout is 300s, not 120s.** The current code
  (`run_ffmpeg` in `lib.rs`) kills the child after `Duration::from_secs(300)`
  → `408 Video processing timed out. Try a shorter or smaller video.`
  `README.md` still says 120s — the 120s value was raised to 300s in commit
  `23ab1d9` (veryfast preset + all cores). Trust 300s.
- **Body-limit layers must cover the largest kind.** The `RequestBodyLimitLayer`
  and `DefaultBodyLimit` are both set from `max_body_bytes()` =
  `max(MAX_IMAGE_MB, MAX_DOCUMENT_MB, MAX_VIDEO_MB)` (+1MiB). If you raise a
  cap, these layers auto-follow (they're computed from the same env). Do NOT
  hardcode a body limit below the largest kind or oversize videos get a
  generic multipart 400 instead of `Maximum video size is 200 MB`.
- **Pre-existing objects have no thumbnail.** Only images uploaded *after*
  the thumbnail feature get a `-thumb.webp`. A `-thumb.webp` request for an
  old image 404s; the consuming frontend must fall back to the full image
  (onerror swap to `.../storage/content/{key}`) so old inventory items keep
  rendering. (Note: list/metadata derive `thumbnail_key` for any image kind, so
  old images advertise a key that 404s on fetch.)
- **Video transcoding is synchronous.** `transcode_video` runs ffmpeg inline
  on the upload request — fine for short product carousel clips (seconds),
  NOT for long course videos (minutes of held request, Cloudflare tunnel open).
  Long-course support needs an async job + status endpoint (designed, not
  built).
- **Silent clips fail the first encode.** A clip with no audio stream fails
  the audio-mapped encode; the code retries once with `-an`. It still counts
  against the 300s budget.
- **Port vs env mismatch.** The binary reads `PORT` (default `8081`), but
  `.env.example`/`.env` and the LXS contract declare `SERVER_PORT` (e.g.
  `26372`) — the binary ignores `SERVER_PORT`. If the estate thinks the
  service listens on `SERVER_PORT`, it's actually on `PORT` (or 8081). The
  `.env` shipped here also sets `CORS_ALLOWED_ORIGINS`, which the code ignores
  (CORS is `Any`).
- **S3 keys are validated as `[A-Za-z0-9._-]` segments** (`safe_segment`,
  ≤120 chars, non-empty). `owner_id`/`namespace`/`reference_id` and each
  `*key` path segment must conform or you get 400 `Invalid storage identifier`
  / `Invalid storage key`. Key generation is `{namespace}/{reference_id}/{uuid}.{ext}`.
- **Bucket is auto-created at startup** if it doesn't exist (`ensure_bucket`).
  Startup hard-fails if `S3_ENDPOINT`/`S3_ACCESS_KEY`/`S3_SECRET_KEY` are
  missing (panics in `bootstrap`). `S3_BUCKET` defaults to `eco-storage`
  (code) — note `.env.example`/contract default it to `stuff8-storage`, so a
  deployment without an explicit `S3_BUCKET` may land objects in an unexpected
  bucket depending on which default applies.
- **Images are re-encoded for storage, not kept verbatim.** Image objects are
  WebP (quality 80) when smaller than the source, JPEG (quality 85) if that's
  smaller, else the original bytes — always downscaled to `IMAGE_MAX_DIMENSION`
  (default 1600) on the long edge. The stored `original_name`/`mime_type` reflect
  the stored form. List/grid views should request the `thumbnail_key` URL
  (`THUMBNAIL_DIMENSION`, default 400), not the full image.
- **Range streaming is single-range only.** A single `Range: bytes=a-b`
  (incl. open-ended `bytes=a-`) gets 206. Suffix ranges (`bytes=-n`) and
  multi-range headers fall back to a full 200 — acceptable for `<video>`
  seeking, which sends single ranges.
