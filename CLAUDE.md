# photos

Reusable storage domain for any Eco estate. It stores images and documents in
S3-compatible MinIO and hands consumers opaque object keys, never S3 details.
Images are converted to WebP when that is smaller; PDF, Word, Excel, and text
documents are stored as-is.

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

They are loaded into `UploadLimits` on `AppState` in `backend/src/main.rs`
and drive both the tower_http/axum body limits and the `process_upload`
rejection messages (which render the configured MB value, not a literal).

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

## API

Base path `/api/storage`. Consumers on the stuff8 estate reach it through the
estate gateway as `/api/...` or the `photos.stuff8.com` hostname
(`expose.additional` in `ecompose.yml`).

- `POST /api/storage/objects` — multipart `file`, `owner_id`, `namespace`,
  `reference_id` → stores and returns object metadata
- `GET /api/storage/objects?owner_id=&namespace=&reference_id=` — list owned
  objects
- `GET /api/storage/content/*key` — retrieve content (images served inline so
  email clients render them)
- `GET /api/storage/objects/*key` — object metadata
- `DELETE /api/storage/objects/*key?owner_id=` — remove an owned object

`owner_id`, `namespace`, `reference_id`, and path segments are validated as
`[A-Za-z0-9._-]` (`safe_segment`); S3 keys stay opaque to consumers.
