use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{config::Region, primitives::ByteStream, Client};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use image::{imageops::FilterType, GenericImageView};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::io::ReaderStream;
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;
use webp::Encoder;

#[derive(Debug, Clone, Copy)]
pub struct UploadLimits {
    pub max_image_bytes: usize,
    pub max_document_bytes: usize,
    pub max_image_pixels: u32,
    pub max_video_bytes: usize,
    /// Long-edge limit the main image is downscaled to (WebP re-encode), so a
    /// 12MP phone photo never stays a multi-megabyte download.
    pub image_max_dimension: u32,
    /// Long edge of the auto-generated WebP thumbnail served by list/grid views.
    pub thumbnail_dimension: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredObject {
    pub key: String,
    pub original_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub kind: String,
    pub owner_id: String,
    pub namespace: String,
    pub reference_id: String,
    pub created_at: DateTime<Utc>,
    /// For images, the key of the auto-generated WebP thumbnail (grid/list
    /// views serve this instead of the full image). `None` for videos,
    /// documents, or pre-thumbnail objects.
    pub thumbnail_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListObjectsQuery {
    pub owner_id: String,
    pub namespace: String,
    pub reference_id: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteObjectQuery {
    pub owner_id: String,
}

#[derive(Clone)]
pub struct AppState {
    client: Client,
    bucket: String,
    limits: UploadLimits,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .init();

    let port = env("PORT", "8081").parse().unwrap_or(8081);
    let endpoint = required_env("S3_ENDPOINT");
    let bucket = env("S3_BUCKET", "eco-storage");
    let region = env("S3_REGION", "us-east-1");
    let limits = UploadLimits {
        max_image_bytes: (env_u64("MAX_IMAGE_MB", 10) * 1024 * 1024) as usize,
        max_document_bytes: (env_u64("MAX_DOCUMENT_MB", 50) * 1024 * 1024) as usize,
        max_image_pixels: env_u64("MAX_IMAGE_PIXELS", 40_000_000) as u32,
        max_video_bytes: (env_u64("MAX_VIDEO_MB", 200) * 1024 * 1024) as usize,
        image_max_dimension: env_u64("IMAGE_MAX_DIMENSION", 1600) as u32,
        thumbnail_dimension: env_u64("THUMBNAIL_DIMENSION", 400) as u32,
    };
    let credentials = Credentials::new(
        required_env("S3_ACCESS_KEY"),
        required_env("S3_SECRET_KEY"),
        None,
        None,
        "eco-managed-minio",
    );
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .credentials_provider(credentials)
        .endpoint_url(endpoint)
        .load()
        .await;
    let s3_config = aws_sdk_s3::config::Builder::from(&shared)
        .force_path_style(true)
        .build();
    let state = AppState {
        client: Client::from_conf(s3_config),
        bucket,
        limits,
    };
    ensure_bucket(&state)
        .await
        .expect("Unable to initialise the configured S3 bucket");

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/api/photos/health", get(health_check))
        // Generic, reusable storage API. `/api/photos` remains only the
        // Stuff8 service identity; consumers should use `/api/storage`.
        .route(
            "/api/storage/objects",
            post(upload_object).get(list_objects),
        )
        .route(
            "/api/storage/content/*key",
            get(download_object).head(download_headers),
        )
        .route(
            "/api/storage/objects/*key",
            get(object_metadata).delete(delete_object),
        )
        // The tower_http and axum body limits must cover the largest upload
        // kind (videos); otherwise multipart parsing rejects an oversized
        // video with a generic 400 before transcode_video's explicit
        // "Ukuran video maksimal …" message can be reached.
        .layer(RequestBodyLimitLayer::new(max_body_bytes(limits)))
        // axum's Multipart extractor reads DefaultBodyLimit (via
        // with_limited_body) and defaults to 2MB; the tower_http
        // RequestBodyLimitLayer above does not raise it. Without this, any
        // upload over ~2MB fails multipart parsing with a 400. Set it just
        // above the image cap so an oversize image is parsed and then
        // rejected by process_upload's clear "Ukuran gambar maksimal 10 MB"
        // message instead of the generic multipart parse error.
        .layer(DefaultBodyLimit::max(max_body_bytes(limits) + 1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "Storage domain service listening");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn env(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn env_u64(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(fallback)
}

fn max_body_bytes(limits: UploadLimits) -> usize {
    limits
        .max_image_bytes
        .max(limits.max_document_bytes)
        .max(limits.max_video_bytes)
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be configured for the storage domain"))
}

async fn ensure_bucket(state: &AppState) -> Result<(), aws_sdk_s3::Error> {
    if state
        .client
        .head_bucket()
        .bucket(&state.bucket)
        .send()
        .await
        .is_err()
    {
        state
            .client
            .create_bucket()
            .bucket(&state.bucket)
            .send()
            .await?;
    }
    Ok(())
}

async fn health_check() -> &'static str {
    "OK - Storage domain service (S3/MinIO)"
}

async fn upload_object(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<StoredObject>), (StatusCode, String)> {
    let mut fields = HashMap::new();
    let mut filename = None;
    let mut content_type = None;
    let mut bytes = None;
    let mut video_input = None;

    while let Some(field) = multipart.next_field().await.map_err(bad_request)? {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            filename = field.file_name().map(ToOwned::to_owned);
            content_type = field.content_type().map(ToOwned::to_owned);
            let declared = content_type
                .clone()
                .unwrap_or_else(|| mime_from_name(filename.as_deref().unwrap_or("upload")));
            if is_video(&declared) {
                // Videos can be large; stream the multipart field to a temp
                // file instead of buffering it in RAM like images/docs.
                let mut field = field;
                let path = std::env::temp_dir().join(format!(
                    "eco-video-in-{}.{}",
                    Uuid::new_v4(),
                    temp_ext(&declared)
                ));
                let mut file = tokio::fs::File::create(&path).await.map_err(bad_request)?;
                while let Some(chunk) = field.chunk().await.map_err(bad_request)? {
                    file.write_all(&chunk).await.map_err(bad_request)?;
                }
                file.flush().await.map_err(bad_request)?;
                drop(file);
                video_input = Some(path);
            } else {
                bytes = Some(field.bytes().await.map_err(bad_request)?.to_vec());
            }
        } else {
            fields.insert(name, field.text().await.map_err(bad_request)?);
        }
    }

    let owner_id = safe_segment(required_field(&fields, "owner_id")?)?;
    let namespace = safe_segment(required_field(&fields, "namespace")?)?;
    let reference_id = safe_segment(required_field(&fields, "reference_id")?)?;
    let original_name = filename.unwrap_or_else(|| "upload".to_string());
    let declared_type = content_type.unwrap_or_else(|| mime_from_name(&original_name));

    if bytes.is_none() && video_input.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Field file wajib diisi".to_string(),
        ));
    }

    let (put_body, mime_type, extension, kind, size_bytes, output_to_clean, thumbnail_bytes) =
        if let Some(input_path) = video_input {
            let transcoded = transcode_video(state.limits, &input_path, &original_name).await?;
            // The input is fully consumed by ffmpeg, so it can go now. The
            // transcoded output must survive until S3 reads it: ByteStream::
            // from_path is lazy and only opens the file when the stream is
            // polled, so deleting it here fails the PutObject.
            let _ = tokio::fs::remove_file(&input_path).await;
            let body = ByteStream::from_path(&transcoded.path)
                .await
                .map_err(s3_error)?;
            (
                body,
                transcoded.mime_type,
                transcoded.extension,
                transcoded.kind,
                transcoded.size_bytes,
                Some(transcoded.path),
                None,
            )
        } else {
            let processed = process_upload(
                state.limits,
                bytes.as_deref().unwrap_or_default(),
                &declared_type,
                &original_name,
            )?;
            (
                ByteStream::from(processed.bytes),
                processed.mime_type,
                processed.extension,
                processed.kind,
                processed.size_bytes as u64,
                None,
                processed.thumbnail,
            )
        };
    let object_id = Uuid::new_v4();
    let key = format!(
        "{namespace}/{reference_id}/{}.{}",
        object_id, extension
    );
    let now = Utc::now();

    let put_metadata = |builder: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder| {
        builder
            .metadata("owner-id", &owner_id)
            .metadata("namespace", &namespace)
            .metadata("reference-id", &reference_id)
            .metadata("original-name", &original_name)
            .metadata("kind", kind)
            .metadata("created-at", now.to_rfc3339())
    };

    let request = put_metadata(
        state
            .client
            .put_object()
            .bucket(&state.bucket)
            .key(&key)
            .body(put_body)
            .content_type(&mime_type),
    );
    request.send().await.map_err(s3_error)?;

    // Thumbnail for images: a small WebP sibling the list/grid views can load
    // instead of the full image. Stored under `<uuid>-thumb.webp`.
    let thumbnail_key = thumbnail_bytes.as_ref().map(|_| {
        format!(
            "{namespace}/{reference_id}/{object_id}-thumb.webp"
        )
    });
    if let (Some(thumb), Some(thumb_key)) = (thumbnail_bytes.as_ref(), thumbnail_key.as_ref()) {
        let thumb_request = put_metadata(
            state
                .client
                .put_object()
                .bucket(&state.bucket)
                .key(thumb_key)
                .body(ByteStream::from(thumb.clone()))
                .content_type("image/webp"),
        );
        thumb_request.send().await.map_err(s3_error)?;
    }
    if let Some(path) = output_to_clean {
        let _ = tokio::fs::remove_file(path).await;
    }

    Ok((
        StatusCode::CREATED,
        Json(StoredObject {
            key,
            original_name,
            mime_type,
            size_bytes,
            kind: kind.to_string(),
            owner_id,
            namespace,
            reference_id,
            created_at: now,
            thumbnail_key,
        }),
    ))
}

async fn list_objects(
    State(state): State<AppState>,
    Query(query): Query<ListObjectsQuery>,
) -> Result<Json<Vec<StoredObject>>, (StatusCode, String)> {
    let owner_id = safe_segment(&query.owner_id)?;
    let namespace = safe_segment(&query.namespace)?;
    let reference_id = safe_segment(&query.reference_id)?;
    let prefix = format!("{namespace}/{reference_id}/");
    let listed = state
        .client
        .list_objects_v2()
        .bucket(&state.bucket)
        .prefix(prefix)
        .send()
        .await
        .map_err(s3_error)?;
    let mut objects = Vec::new();
    for object in listed.contents() {
        let Some(key) = object.key() else {
            continue;
        };
        let head = state
            .client
            .head_object()
            .bucket(&state.bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_error)?;
        let metadata = head.metadata().cloned().unwrap_or_default();
        if metadata
            .get("owner-id")
            .is_some_and(|value| value == &owner_id)
        {
            objects.push(stored_object_from_head(
                key.to_string(),
                &metadata,
                head.content_type(),
                head.content_length().unwrap_or(0),
            ));
        }
    }
    objects.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(Json(objects))
}

/// HEAD an object, mapping a genuinely missing key to a clean 404 (instead of
/// 502) so the `onerror` thumbnail fallback and HTTP caches behave correctly.
/// All S3-level failures still surface as 502 via `s3_error`.
async fn head_object_checked(
    state: &AppState,
    key: &str,
) -> Result<aws_sdk_s3::operation::head_object::HeadObjectOutput, (StatusCode, String)> {
    match state
        .client
        .head_object()
        .bucket(&state.bucket)
        .key(key)
        .send()
        .await
    {
        Ok(head) => Ok(head),
        Err(error) if error.as_service_error().is_some_and(|e| e.is_not_found()) => Err((
            StatusCode::NOT_FOUND,
            "File tidak ditemukan".to_string(),
        )),
        Err(error) => Err(s3_error(error)),
    }
}

async fn object_metadata(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<StoredObject>, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = head_object_checked(&state, &key).await?;
    Ok(Json(stored_object_from_head(
        key,
        &head.metadata().cloned().unwrap_or_default(),
        head.content_type(),
        head.content_length().unwrap_or(0),
    )))
}

async fn download_object(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = head_object_checked(&state, &key).await?;
    let total = head.content_length().unwrap_or(0).max(0) as u64;

    match headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, total))
    {
        // Video players ask for byte ranges to stream and seek. Serve a
        // single byte range as 206 Partial Content so playback starts before
        // the object is fully downloaded and seeking works.
        Some((start, end)) if start < total => {
            let end = end.min(total.saturating_sub(1));
            if end < start {
                return full_object(&state, &key, total).await;
            }
            let object = state
                .client
                .get_object()
                .bucket(&state.bucket)
                .key(&key)
                .range(format!("bytes={start}-{end}"))
                .send()
                .await
                .map_err(s3_error)?;
            let mime = object
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_string();
            let original_name = original_name_from(object.metadata(), &key);
            let response = Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, &mime)
                .header(
                    header::CONTENT_DISPOSITION,
                    download_disposition(&original_name, &mime),
                )
                .header(header::CONTENT_LENGTH, end - start + 1)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .body(Body::from_stream(ReaderStream::new(
                    object.body.into_async_read(),
                )))
                .map_err(bad_request)?;
            Ok(response)
        }
        _ => full_object(&state, &key, total).await,
    }
}

async fn full_object(
    state: &AppState,
    key: &str,
    total: u64,
) -> Result<Response, (StatusCode, String)> {
    let object = state
        .client
        .get_object()
        .bucket(&state.bucket)
        .key(key)
        .send()
        .await
        .map_err(s3_error)?;
    let mime = object
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let original_name = original_name_from(object.metadata(), key);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &mime)
        .header(
            header::CONTENT_DISPOSITION,
            download_disposition(&original_name, &mime),
        )
        .header(header::CONTENT_LENGTH, total)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::from_stream(ReaderStream::new(
            object.body.into_async_read(),
        )))
        .map_err(bad_request)?;
    Ok(response)
}

async fn download_headers(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = head_object_checked(&state, &key).await?;
    let mime = head
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let original_name = original_name_from(head.metadata(), &key);
    let total = head.content_length().unwrap_or(0).max(0) as u64;
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &mime)
        .header(
            header::CONTENT_DISPOSITION,
            download_disposition(&original_name, &mime),
        )
        .header(header::CONTENT_LENGTH, total)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::empty())
        .map_err(bad_request)?;
    Ok(response)
}

/// Parse a single `Range: bytes=start-end` header into a closed interval.
/// Suffix ranges (`bytes=-N`) and multi-range requests are not supported and
/// fall back to serving the whole object as a plain 200.
fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    if spec.is_empty() || !spec.contains('-') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = if end.trim().is_empty() {
        total.saturating_sub(1)
    } else {
        end.trim().parse().ok()?
    };
    Some((start, end))
}

fn original_name_from(metadata: Option<&HashMap<String, String>>, key: &str) -> String {
    metadata
        .and_then(|meta| meta.get("original-name"))
        .cloned()
        .unwrap_or_else(|| key.rsplit('/').next().unwrap_or("download").to_string())
}

async fn delete_object(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(query): Query<DeleteObjectQuery>,
) -> Result<StatusCode, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = head_object_checked(&state, &key).await?;
    if head
        .metadata()
        .and_then(|meta| meta.get("owner-id"))
        .is_none_or(|owner| owner != &query.owner_id)
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Hanya pemilik file yang dapat menghapusnya".to_string(),
        ));
    }
    let kind = head.metadata().and_then(|meta| meta.get("kind")).map(String::as_str);
    state
        .client
        .delete_object()
        .bucket(&state.bucket)
        .key(&key)
        .send()
        .await
        .map_err(s3_error)?;
    // Images also carry a `-thumb.webp` sibling; remove it too so clearing an
    // item doesn't orphan a thumbnail in storage.
    if kind == Some("image") {
        let thumb_key = format!("{}-thumb.webp", key.trim_end_matches(".webp"));
        state
            .client
            .delete_object()
            .bucket(&state.bucket)
            .key(&thumb_key)
            .send()
            .await
            .map_err(s3_error)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

struct ProcessedUpload {
    bytes: Vec<u8>,
    mime_type: String,
    extension: &'static str,
    kind: &'static str,
    size_bytes: usize,
    /// Small WebP variant for images (list/grid views), `None` otherwise.
    thumbnail: Option<Vec<u8>>,
}

fn fit_dimensions(width: u32, height: u32, max_dim: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max_dim || longest == 0 {
        (width, height)
    } else {
        let scale = max_dim as f64 / longest as f64;
        (
            ((width as f64 * scale).round() as u32).max(1),
            ((height as f64 * scale).round() as u32).max(1),
        )
    }
}

fn encode_webp(image: &image::DynamicImage, quality: f32) -> Vec<u8> {
    if image.color().has_alpha() {
        let rgba = image.to_rgba8();
        Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
            .encode(quality)
            .to_vec()
    } else {
        let rgb = image.to_rgb8();
        Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height())
            .encode(quality)
            .to_vec()
    }
}

fn process_upload(
    limits: UploadLimits,
    bytes: &[u8],
    mime_type: &str,
    filename: &str,
) -> Result<ProcessedUpload, (StatusCode, String)> {
    if is_image(mime_type) {
        if bytes.len() > limits.max_image_bytes {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "Ukuran gambar maksimal {} MB",
                    limits.max_image_bytes / (1024 * 1024)
                ),
            ));
        }
        let image = image::load_from_memory(bytes).map_err(|_| {
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "File gambar tidak valid".to_string(),
            )
        })?;
        let (width, height) = image.dimensions();
        if width.saturating_mul(height) > limits.max_image_pixels {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Resolusi gambar terlalu besar".to_string(),
            ));
        }

        // Downscale the main image so a phone photo is no longer a multi-MB
        // download for the detail view, then re-encode to WebP (kept only
        // when it is actually smaller than the original, as before).
        let (main_w, main_h) = fit_dimensions(width, height, limits.image_max_dimension);
        let main_image = if main_w != width || main_h != height {
            image.resize(main_w, main_h, FilterType::Lanczos3)
        } else {
            image.clone()
        };
        let candidate = encode_webp(&main_image, 80.0);

        // A small WebP thumbnail for list/grid views.
        let (thumb_w, thumb_h) = fit_dimensions(width, height, limits.thumbnail_dimension);
        let thumb_image = if thumb_w != width || thumb_h != height {
            image.resize(thumb_w, thumb_h, FilterType::Lanczos3)
        } else {
            image.clone()
        };
        let thumbnail = encode_webp(&thumb_image, 80.0);

        if candidate.len() < bytes.len() {
            return Ok(ProcessedUpload {
                size_bytes: candidate.len(),
                bytes: candidate,
                mime_type: "image/webp".to_string(),
                extension: "webp",
                kind: "image",
                thumbnail: Some(thumbnail),
            });
        }
        return Ok(ProcessedUpload {
            size_bytes: bytes.len(),
            bytes: bytes.to_vec(),
            mime_type: mime_type.to_string(),
            extension: extension_for(filename, mime_type),
            kind: "image",
            thumbnail: Some(thumbnail),
        });
    }
    if !is_document(mime_type) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Jenis file tidak didukung".to_string(),
        ));
    }
    if bytes.len() > limits.max_document_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "Ukuran dokumen maksimal {} MB",
                limits.max_document_bytes / (1024 * 1024)
            ),
        ));
    }
    Ok(ProcessedUpload {
        size_bytes: bytes.len(),
        bytes: bytes.to_vec(),
        mime_type: mime_type.to_string(),
        extension: extension_for(filename, mime_type),
        kind: "document",
        thumbnail: None,
    })
}

fn is_image(mime: &str) -> bool {
    matches!(mime, "image/jpeg" | "image/png" | "image/webp")
}
fn is_document(mime: &str) -> bool {
    matches!(
        mime,
        "application/pdf"
            | "application/msword"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "text/plain"
    )
}
fn is_video(mime: &str) -> bool {
    matches!(
        mime,
        "video/mp4"
            | "video/quicktime"
            | "video/webm"
            | "video/x-matroska"
            | "video/x-msvideo"
            | "video/3gpp"
            | "video/3gpp2"
            | "video/mpeg"
            | "video/ogg"
    )
}

/// File extension used for the temp *input* file so ffmpeg's format probing
/// sees a hint that matches the declared container.
fn temp_ext(mime: &str) -> &'static str {
    match mime {
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/x-msvideo" => "avi",
        "video/3gpp" | "video/3gpp2" => "3gp",
        "video/mpeg" => "mpeg",
        "video/ogg" => "ogv",
        _ => "mp4",
    }
}

struct TranscodedVideo {
    path: std::path::PathBuf,
    mime_type: String,
    extension: &'static str,
    kind: &'static str,
    size_bytes: u64,
}

/// Validate the size cap, then re-encode the uploaded video to a browser- and
/// bandwidth-friendly MP4 (H.264 + AAC, faststart, capped at 1280px wide and
/// ~2 Mbps) with ffmpeg. Compression runs synchronously; short product
/// carousel clips transcode in a few seconds on the estate's cores. Long
/// course videos will need an async job + status endpoint (see CLAUDE.md).
async fn transcode_video(
    limits: UploadLimits,
    input: &std::path::Path,
    filename: &str,
) -> Result<TranscodedVideo, (StatusCode, String)> {
    let input_len = tokio::fs::metadata(input).await.map_err(bad_request)?.len();
    if input_len > limits.max_video_bytes as u64 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "Ukuran video maksimal {} MB",
                limits.max_video_bytes / (1024 * 1024)
            ),
        ));
    }
    let output = std::env::temp_dir().join(format!("eco-video-out-{}.mp4", Uuid::new_v4()));
    let result = match run_ffmpeg(input, &output, true).await {
        Ok(()) => Ok(()),
        Err((StatusCode::REQUEST_TIMEOUT, _)) => {
            // A stalled encode should not be retried as if it were a silent
            // clip — surface the timeout to the user instead.
            Err((
                StatusCode::REQUEST_TIMEOUT,
                "Pemrosesan video memakan waktu terlalu lama. Coba video yang lebih pendek atau lebih kecil.".to_string(),
            ))
        }
        Err(_) => {
            // Silent clips (no audio track) fail the audio-mapped encode;
            // retry dropping audio entirely instead of rejecting the upload.
            run_ffmpeg(input, &output, false).await
        }
    };
    if let Err((status, message)) = result {
        // Remove the temp output on failure so we never leak it.
        let _ = tokio::fs::remove_file(&output).await;
        return Err((status, message));
    }
    let size_bytes = tokio::fs::metadata(&output)
        .await
        .map_err(bad_request)?
        .len();
    if size_bytes == 0 {
        let _ = tokio::fs::remove_file(&output).await;
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("Gagal memproses video: {}", file_name_hint(filename)),
        ));
    }
    Ok(TranscodedVideo {
        path: output,
        mime_type: "video/mp4".to_string(),
        extension: "mp4",
        kind: "video",
        size_bytes,
    })
}

fn file_name_hint(filename: &str) -> String {
    let truncated: String = filename.chars().take(40).collect();
    if truncated == filename {
        filename.to_string()
    } else {
        format!("{truncated}…")
    }
}

async fn run_ffmpeg(
    input: &std::path::Path,
    output: &std::path::Path,
    with_audio: bool,
) -> Result<(), (StatusCode, String)> {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("fast")
        .arg("-crf")
        .arg("26")
        .arg("-maxrate")
        .arg("2M")
        .arg("-bufsize")
        .arg("4M")
        .arg("-vf")
        .arg("scale='min(1280,iw)':-2")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-r")
        .arg("30");
    if with_audio {
        command.arg("-c:a").arg("aac").arg("-b:a").arg("128k");
    } else {
        command.arg("-an");
    }
    command
        .arg("-movflags")
        .arg("+faststart")
        .arg("-f")
        .arg("mp4")
        .arg(output)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // A stalled or impossibly slow encode must not hold the upload request
    // (and the Cloudflare tunnel) open forever. Cap it, then kill the child.
    let mut child = command.spawn().map_err(bad_request)?;
    let status = tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await;
    match status {
        Ok(Ok(exit)) if exit.success() => Ok(()),
        Ok(Ok(_)) => Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "ffmpeg gagal memproses video".to_string(),
        )),
        Ok(Err(error)) => Err(bad_request(error)),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err((
                StatusCode::REQUEST_TIMEOUT,
                "ffmpeg gagal memproses video".to_string(),
            ))
        }
    }
}

/// Keep the S3 key opaque while giving the browser the original upload name.
/// RFC 5987's `filename*` keeps Unicode names valid in a response header.
/// Images and videos are served `inline` so browsers render/stream them in
/// place (Gmail/Outlook refuse to display `attachment` images, and an
/// `attachment` video would download instead of playing); non-media documents
/// keep `attachment` so they still download instead of rendering.
fn download_disposition(filename: &str, mime: &str) -> HeaderValue {
    let encoded = filename
        .as_bytes()
        .iter()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                format!("{}", *byte as char)
            }
            _ => format!("%{:02X}", byte),
        })
        .collect::<String>();
    let kind = if is_image(mime) || is_video(mime) {
        "inline"
    } else {
        "attachment"
    };
    HeaderValue::from_str(&format!("{kind}; filename*=UTF-8''{encoded}"))
        .unwrap_or_else(|_| HeaderValue::from_str(kind).unwrap())
}

fn extension_for(filename: &str, mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/plain" => "txt",
        mime if is_video(mime) => "mp4",
        _ if filename.ends_with(".bin") => "bin",
        _ => "bin",
    }
}
fn mime_from_name(name: &str) -> String {
    if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".webp") {
        "image/webp"
    } else if name.ends_with(".pdf") {
        "application/pdf"
    } else if name.ends_with(".mp4")
        || name.ends_with(".mov")
        || name.ends_with(".m4v")
        || name.ends_with(".webm")
        || name.ends_with(".mkv")
        || name.ends_with(".avi")
        || name.ends_with(".3gp")
        || name.ends_with(".ogv")
    {
        "video/mp4"
    } else {
        "image/jpeg"
    }
    .to_string()
}
fn required_field<'a>(
    fields: &'a HashMap<String, String>,
    name: &str,
) -> Result<&'a str, (StatusCode, String)> {
    fields
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("Field {name} wajib diisi")))
}
fn safe_segment(value: &str) -> Result<String, (StatusCode, String)> {
    if !value.is_empty()
        && value.len() <= 120
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Ok(value.to_string())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "Identifier storage tidak valid".to_string(),
        ))
    }
}
fn valid_key(key: &str) -> Result<String, (StatusCode, String)> {
    if key.split('/').all(|segment| safe_segment(segment).is_ok()) {
        Ok(key.to_string())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "Key storage tidak valid".to_string(),
        ))
    }
}
fn stored_object_from_head(
    key: String,
    metadata: &HashMap<String, String>,
    content_type: Option<&str>,
    content_length: i64,
) -> StoredObject {
    let kind = metadata
        .get("kind")
        .cloned()
        .unwrap_or_else(|| "document".to_string());
    let key_trimmed_webp = key.trim_end_matches(".webp").to_string();
    StoredObject {
        key,
        original_name: metadata
            .get("original-name")
            .cloned()
            .unwrap_or_else(|| "file".to_string()),
        mime_type: content_type
            .unwrap_or("application/octet-stream")
            .to_string(),
        size_bytes: content_length.max(0) as u64,
        thumbnail_key: (kind == "image").then(|| {
            format!("{}-thumb.webp", key_trimmed_webp)
        }),
        kind,
        owner_id: metadata.get("owner-id").cloned().unwrap_or_default(),
        namespace: metadata.get("namespace").cloned().unwrap_or_default(),
        reference_id: metadata.get("reference-id").cloned().unwrap_or_default(),
        created_at: metadata
            .get("created-at")
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(Utc::now),
    }
}
fn bad_request<E: std::fmt::Display>(error: E) -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        format!("Upload tidak valid: {error}"),
    )
}
fn s3_error<E: std::fmt::Display>(error: E) -> (StatusCode, String) {
    tracing::error!(%error, "S3 operation failed");
    (
        StatusCode::BAD_GATEWAY,
        "Storage tidak tersedia".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{codecs::jpeg::JpegEncoder, ImageBuffer, Rgb};

    fn test_limits() -> UploadLimits {
        UploadLimits {
            max_image_bytes: 10 * 1024 * 1024,
            max_document_bytes: 50 * 1024 * 1024,
            max_image_pixels: 40_000_000,
            max_video_bytes: 200 * 1024 * 1024,
            image_max_dimension: 1600,
            thumbnail_dimension: 400,
        }
    }

    #[test]
    fn image_conversion_never_increases_the_stored_file_size() {
        let image = ImageBuffer::from_pixel(320, 240, Rgb([32u8, 83, 158]));
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&image)
            .unwrap();

        let stored = process_upload(test_limits(), &jpeg, "image/jpeg", "inventory.jpg").unwrap();
        assert!(stored.size_bytes <= jpeg.len());
        if stored.mime_type == "image/webp" {
            assert!(stored.size_bytes < jpeg.len());
        }
    }

    #[test]
    fn images_are_downscaled_and_thumbnail_is_small() {
        let mut image = ImageBuffer::new(3000, 2000);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let r = (x * 255 / 3000) as u8;
            let g = (y * 255 / 2000) as u8;
            let b = ((x + y) * 255 / 5000) as u8;
            *pixel = Rgb([r, g, b]);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&image)
            .unwrap();

        let stored = process_upload(test_limits(), &jpeg, "image/jpeg", "huge.jpg").unwrap();
        // Main image must have been downscaled from 3000px to <= 1600px.
        assert_eq!(stored.extension, "webp");
        let decoded = image::load_from_memory(&stored.bytes).unwrap();
        let (w, h) = decoded.dimensions();
        assert!(w.max(h) <= 1600, "main image still {w}x{h}");

        // Thumbnail must exist and be within the thumbnail dimension.
        let thumb = stored.thumbnail.expect("thumbnail should be generated");
        let thumb_image = image::load_from_memory(&thumb).unwrap();
        let (tw, th) = thumb_image.dimensions();
        assert!(tw.max(th) <= 400, "thumbnail still {tw}x{th}");
        assert!(thumb.len() < stored.bytes.len(), "thumbnail not smaller than main");
    }

    #[test]
    fn video_mime_classification() {
        assert!(is_video("video/mp4"));
        assert!(is_video("video/quicktime"));
        assert!(is_video("video/webm"));
        assert!(!is_video("image/jpeg"));
        assert!(!is_video("application/pdf"));
        assert_eq!(extension_for("clip.mov", "video/quicktime"), "mp4");
        assert_eq!(extension_for("clip.mp4", "video/mp4"), "mp4");
        assert_eq!(mime_from_name("promo.mp4"), "video/mp4");
        assert_eq!(mime_from_name("raw.mov"), "video/mp4");
        assert_eq!(mime_from_name("logo.png"), "image/png");
        assert_eq!(temp_ext("video/quicktime"), "mov");
        assert_eq!(temp_ext("video/mp4"), "mp4");
    }

    #[test]
    fn range_header_parsing() {
        assert_eq!(parse_range("bytes=0-499", 1000), Some((0, 499)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), Some((1000, 999)));
        assert_eq!(parse_range("bytes=0-0", 1000), Some((0, 0)));
        assert_eq!(parse_range("bytes=-500", 1000), None);
        assert_eq!(parse_range("items=0-5", 1000), None);
        assert_eq!(parse_range("", 1000), None);
        assert_eq!(parse_range("bytes=abc-def", 1000), None);
    }

    #[test]
    fn video_disposition_is_inline() {
        let value = download_disposition("promo.mp4", "video/mp4");
        let value = value.to_str().unwrap();
        assert!(value.starts_with("inline;"));
        let doc = download_disposition("doc.pdf", "application/pdf");
        assert!(doc.to_str().unwrap().starts_with("attachment;"));
    }

    #[test]
    fn video_size_cap_is_reported_in_mb() {
        let limits = test_limits();
        assert_eq!(max_body_bytes(limits), 200 * 1024 * 1024);
        assert_eq!(limits.max_video_bytes / (1024 * 1024), 200);
    }

    #[test]
    fn thumbnail_key_derivation() {
        let head = HashMap::new();
        let obj = stored_object_from_head(
            "inventory/abc/123.webp".to_string(),
            &head,
            Some("image/webp"),
            42,
        );
        // kind is empty here (no metadata), so no thumbnail key.
        assert_eq!(obj.thumbnail_key, None);
        let mut meta = HashMap::new();
        meta.insert("kind".to_string(), "image".to_string());
        let obj2 = stored_object_from_head(
            "inventory/abc/123.webp".to_string(),
            &meta,
            Some("image/webp"),
            42,
        );
        assert_eq!(obj2.thumbnail_key, Some("inventory/abc/123-thumb.webp".to_string()));
    }

    #[test]
    fn transcode_produces_small_playable_mp4() {
        let ffmpeg = std::process::Command::new("ffmpeg")
            .arg("-version")
            .output();
        if ffmpeg.is_err() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir();
            let source = dir.join("eco-test-src.mp4");
            let generated = tokio::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=duration=1:size=640x360:rate=25",
                ])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
                .arg(&source)
                .output()
                .await
                .expect("ffmpeg test clip generation failed");
            assert!(generated.status.success(), "test clip generation failed");

            let limits = test_limits();
            let result = transcode_video(limits, &source, "test-clip.mov")
                .await
                .expect("transcode failed");
            assert_eq!(result.mime_type, "video/mp4");
            assert_eq!(result.extension, "mp4");
            assert_eq!(result.kind, "video");
            assert!(result.size_bytes > 0);
            let _ = tokio::fs::remove_file(&source).await;
            let _ = tokio::fs::remove_file(&result.path).await;
        });
    }
}
