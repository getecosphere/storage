# storage API

Base path: `/api/storage`. Auth: **none** — every endpoint is open. Errors are
plain-text bodies (not JSON): each handler returns `(StatusCode, String)` and
axum serializes it as `text/plain`. Success bodies are JSON (the `StoredObject`
struct — serialized with **snake_case** field names, e.g. `thumbnail_key`,
`original_name`; the struct has no `serde(rename_all)`).

The service also exposes `/health` and `/api/photos/health` (legacy path) for
liveness.

## Endpoints

### GET /health  ·  GET /api/photos/health
- **Purpose:** liveness probe (the `/api/photos/health` path is kept for
  backward compatibility with the pre-rename domain).
- **Auth required:** no
- **Success 200:** `text/plain` body: `OK - Storage domain service (S3/MinIO)`

### POST /api/storage/objects
- **Purpose:** store an uploaded file. Images are downscaled to at most
  `IMAGE_MAX_DIMENSION` px on the long edge, then stored as WebP (quality 80)
  *when the WebP is smaller than the source* (otherwise JPEG quality 85 if
  smaller, else the original bytes), always producing an image object plus a
  `-thumb.webp` sibling sized by `THUMBNAIL_DIMENSION`. Documents are stored
  as-is. Videos are transcoded to MP4 (H.264/AAC) with `ffmpeg`. Kind is
  `image` | `document` | `video`. Returns the opaque object key and metadata.
- **Auth required:** no
- **Body:** `multipart/form-data`:
  - `file` (required) — the bytes; MIME type from the part header (or inferred
    from the filename). Accepted: `image/jpeg`, `image/png`, `image/webp`;
    `application/pdf`, `application/msword` (.doc),
    `application/vnd.openxmlformats-officedocument.wordprocessingml.document`
    (.docx), `application/vnd.ms-excel` (.xls),
    `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet` (.xlsx),
    `text/plain`; video MIME types (`video/mp4`, `video/quicktime`,
    `video/webm`, `video/x-matroska`, `video/x-msvideo`, `video/3gpp`,
    `video/3gpp2`, `video/mpeg`, `video/ogg`). Anything else → 415.
  - `owner_id` (required) — `[A-Za-z0-9._-]`, ≤ 120 chars
  - `namespace` (required) — same segment rules
  - `reference_id` (required) — same segment rules
  - (extra text fields are ignored)
- **Success 201:** `StoredObject`:
  ```json
  {
    "key": "inventory/item-42/6f1e9f2b-a1c4-4b8d-9e0f-1a2b3c4d5e6f.webp",
    "original_name": "photo.jpg",
    "mime_type": "image/webp",
    "size_bytes": 123456,
    "kind": "image",
    "owner_id": "u_123",
    "namespace": "inventory",
    "reference_id": "item-42",
    "created_at": "2026-08-13T00:00:00Z",
    "thumbnail_key": "inventory/item-42/6f1e9f2b-a1c4-4b8d-9e0f-1a2b3c4d5e6f-thumb.webp"
  }
  ```
  `thumbnail_key` is present only for `kind: "image"` (null/absent for
  documents and videos).
- **Errors:**
  - 400 `Field file wajib diisi` — no `file` part
  - 400 `Field owner_id wajib diisi` (similarly namespace/reference_id) — a
    required text field missing/blank
  - 400 `Invalid storage identifier` — a segment fails `[A-Za-z0-9._-]`/length
  - 400 `Invalid upload: ...` — multipart read failure, temp-file failure, or
    `ffmpeg` not installed (Command::new error)
  - 413 `Maximum image size is 10 MB` — image > `MAX_IMAGE_MB`
  - 413 `Resolusi gambar terlalu besar` — width×height > `MAX_IMAGE_PIXELS`
  - 413 `Maximum document size is 50 MB` — document > `MAX_DOCUMENT_MB`
  - 413 `Maximum video size is 200 MB` — uploaded video > `MAX_VIDEO_MB`
    (before ffmpeg compresses it)
  - 415 `Invalid image file` — declared image but undecodable
  - 415 `Unsupported file type` — MIME not in the accepted sets
  - 415 `ffmpeg failed to process the video` — ffmpeg exited non-zero (incl.
    silent-clip audio-mapped failure retried with `-an`)
  - 408 `Video processing timed out. Try a shorter or smaller video.` —
    ffmpeg exceeded its 300s timeout (child is killed)
  - 502 `Storage unavailable` — S3 operation failed

### GET /api/storage/objects?owner_id=&namespace=&reference_id=
- **Purpose:** list objects in a `{namespace}/{reference_id}/` prefix that
  belong to `owner_id` (matched against stored `owner-id` metadata), sorted by
  `created_at` ascending.
- **Auth required:** no
- **Query params:** `owner_id`, `namespace`, `reference_id` (all required,
  same segment rules). Note the owner filter applies *in addition* to the
  prefix — an object under the prefix uploaded by another owner is excluded.
- **Success 200:** array of `StoredObject` (each with the same shape as the
  upload response; `thumbnail_key` derived for images). Empty array if none.
- **Errors:** 400 `Invalid storage identifier`; 502 `Storage unavailable`

### GET /api/storage/content/*key
- **Purpose:** retrieve object bytes. Range-aware: a single
  `Range: bytes=start-end` header → **206** with `Content-Range` and
  `Accept-Ranges: bytes`, streamed from S3 (`into_async_read()` +
  `ReaderStream`, never buffered into RAM) — `<video src>` starts instantly
  and seeks. `Range: bytes=start-` (open-ended) is honored; suffix/unsupported
  or multi-range headers fall back to a full **200**. Images and videos are
  served `Content-Disposition: inline` (browsers render/play in place);
  documents are `attachment` with a `filename*=UTF-8''...` header. The
  original uploaded filename is preserved.
- **Auth required:** no
- **Path:** `*key` — the full object key (slashes fine); every segment must
  pass `safe_segment`.
- **Success:** 200 (full object, `Content-Length`, `Accept-Ranges: bytes`) or
  206 (partial, `Content-Range: bytes start-end/total`).
- **Errors:**
  - 400 `Invalid storage key`
  - 404 `File not found` (missing object → 404, not 502 — fixed so thumbnail
    `onerror` fallbacks and caches behave)
  - 502 `Storage unavailable`

### HEAD /api/storage/content/*key
- **Purpose:** probe headers for a key without the body (same headers as GET:
  content-type, content-disposition, content-length, accept-ranges). Used by
  `<video>`/`<audio>` for probing.
- **Auth required:** no
- **Success 200:** headers only, empty body. Errors as GET.

### GET /api/storage/objects/*key
- **Purpose:** fetch an object's metadata (reads S3 head, maps metadata back
  to `StoredObject`).
- **Auth required:** no
- **Path:** `*key` (segment-validated)
- **Success 200:** `StoredObject`
- **Errors:** 400 `Invalid storage key`; 404 `File not found`; 502 `Storage unavailable`

### DELETE /api/storage/objects/*key?owner_id=
- **Purpose:** delete an object. For `kind: "image"` also deletes the
  `-thumb.webp` sibling (key derived by stripping the extension and appending
  `-thumb.webp`). Caller must pass the same `owner_id` the object was uploaded
  with.
- **Auth required:** no
- **Path:** `*key`; **Query:** `owner_id` (required)
- **Success 204:** empty body
- **Errors:**
  - 400 `Invalid storage key` / `Invalid storage identifier`
  - 403 `Hanya pemilik file yang dapat menghapusnya` — `owner_id` does not
    match the object's stored `owner-id`
  - 404 `File not found` — object does not exist
  - 502 `Storage unavailable`

## StoredObject (response shape)

```json
{
  "key": "string",
  "original_name": "string",
  "mime_type": "string",
  "size_bytes": 0,
  "kind": "image|document|video",
  "owner_id": "string",
  "namespace": "string",
  "reference_id": "string",
  "created_at": "2026-08-13T00:00:00Z",
  "thumbnail_key": "string|null"
}
```

`created_at` (response field) is stored as S3 user-metadata `created-at`
(RFC3339) and read back on list/metadata.

## Error reference

| Status | Body (text/plain) | When |
|---|---|---|
| 400 | `Field file wajib diisi` | Missing `file` part |
| 400 | `Field owner_id wajib diisi` (etc.) | Missing/blank required field |
| 400 | `Invalid storage identifier` | Bad `owner_id`/`namespace`/`reference_id` |
| 400 | `Invalid storage key` | Bad path segment in `*key` |
| 400 | `Invalid upload: ...` | Multipart/temp-file failure, or ffmpeg missing |
| 403 | `Hanya pemilik file yang dapat menghapusnya` | Delete with wrong `owner_id` |
| 404 | `File not found` | Object (or thumbnail) does not exist |
| 408 | `Video processing timed out. Try a shorter or smaller video.` | ffmpeg > 300s timeout |
| 413 | `Maximum image size is 10 MB` | Image > `MAX_IMAGE_MB` |
| 413 | `Resolusi gambar terlalu besar` | Image pixels > `MAX_IMAGE_PIXELS` |
| 413 | `Maximum document size is 50 MB` | Document > `MAX_DOCUMENT_MB` |
| 413 | `Maximum video size is 200 MB` | Video > `MAX_VIDEO_MB` (pre-transcode) |
| 415 | `Invalid image file` | Image MIME but undecodable bytes |
| 415 | `Unsupported file type` | MIME not accepted |
| 415 | `ffmpeg failed to process the video` | ffmpeg non-zero exit (retry with `-an` already attempted) |
| 502 | `Storage unavailable` | S3/MinIO operation failure |

The 413 messages render the configured MB value (e.g. `MAX_IMAGE_MB=20` →
"Maximum image size is 20 MB"), not a literal.

## Rate limiting / limits

- **No rate limiting** — none documented in the code.
- **CORS:** wide open — `CorsLayer::new().allow_origin(Any).allow_methods(Any)
  .allow_headers(Any)`. Do not rely on CORS for protection; there is no auth.
- **Request body limit:** the tower_http `RequestBodyLimitLayer` and axum
  `DefaultBodyLimit` are both set to `max_body_bytes()` =
  `max(MAX_IMAGE_MB, MAX_DOCUMENT_MB, MAX_VIDEO_MB)` bytes (+1 MiB headroom on
  the axum layer). This always covers the largest configured kind.
- **Storage keys:** `{namespace}/{reference_id}/{uuid}.{ext}` — consumers
  treat them as opaque.

## Environment variables

Read in `bootstrap()` (and the port in `main.rs`):

| Var | Default | Notes |
|---|---|---|
| `PORT` | `8081` | HTTP listen port (this is the var the binary actually reads) |
| `S3_ENDPOINT` | — required | MinIO/S3 endpoint URL |
| `S3_ACCESS_KEY` | — required | |
| `S3_SECRET_KEY` | — required | |
| `S3_BUCKET` | `eco-storage` | auto-created at startup if missing |
| `S3_REGION` | `us-east-1` | |
| `MAX_IMAGE_MB` | `10` | image size cap |
| `MAX_DOCUMENT_MB` | `50` | document size cap |
| `MAX_IMAGE_PIXELS` | `40000000` | width × height cap |
| `MAX_VIDEO_MB` | `200` | cap on the *uploaded* video, before ffmpeg |
| `IMAGE_MAX_DIMENSION` | `1600` | long-edge downscale for stored images |
| `THUMBNAIL_DIMENSION` | `400` | long-edge size for `-thumb.webp` |

`.env.example` additionally lists `SERVER_PORT`, `API_BASE_URL`,
`CORS_ALLOWED_ORIGINS` (the estate compose contract declares `SERVER_PORT`/
`API_BASE_URL` as required) — note `SERVER_PORT` and `CORS_ALLOWED_ORIGINS`
are **not read by the binary** (the port comes from `PORT`; CORS is `Any`).
