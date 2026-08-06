use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{config::Region, primitives::ByteStream, Client};
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr};
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;
use webp::Encoder;

const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 50 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u32 = 40_000_000;

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
        .route("/api/storage/content/*key", get(download_object))
        .route(
            "/api/storage/objects/*key",
            get(object_metadata).delete(delete_object),
        )
        .layer(RequestBodyLimitLayer::new(MAX_DOCUMENT_BYTES))
        // axum's Multipart extractor reads DefaultBodyLimit (via
        // with_limited_body) and defaults to 2MB; the tower_http
        // RequestBodyLimitLayer above does not raise it. Without this, any
        // upload over ~2MB fails multipart parsing with a 400. Set it just
        // above the image cap so an oversize image is parsed and then
        // rejected by process_upload's clear "Ukuran gambar maksimal 5 MB"
        // message instead of the generic multipart parse error.
        .layer(DefaultBodyLimit::max(MAX_IMAGE_BYTES + 1024 * 1024))
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

    while let Some(field) = multipart.next_field().await.map_err(bad_request)? {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            filename = field.file_name().map(ToOwned::to_owned);
            content_type = field.content_type().map(ToOwned::to_owned);
            bytes = Some(field.bytes().await.map_err(bad_request)?.to_vec());
        } else {
            fields.insert(name, field.text().await.map_err(bad_request)?);
        }
    }

    let owner_id = safe_segment(required_field(&fields, "owner_id")?)?;
    let namespace = safe_segment(required_field(&fields, "namespace")?)?;
    let reference_id = safe_segment(required_field(&fields, "reference_id")?)?;
    let original_name = filename.unwrap_or_else(|| "upload".to_string());
    let input = bytes.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "Field file wajib diisi".to_string(),
        )
    })?;
    let declared_type = content_type.unwrap_or_else(|| mime_from_name(&original_name));
    let processed = process_upload(&input, &declared_type, &original_name)?;
    let key = format!(
        "{namespace}/{reference_id}/{}.{}",
        Uuid::new_v4(),
        processed.extension
    );
    let now = Utc::now();

    state
        .client
        .put_object()
        .bucket(&state.bucket)
        .key(&key)
        .body(ByteStream::from(processed.bytes))
        .content_type(&processed.mime_type)
        .metadata("owner-id", &owner_id)
        .metadata("namespace", &namespace)
        .metadata("reference-id", &reference_id)
        .metadata("original-name", &original_name)
        .metadata("kind", processed.kind)
        .metadata("created-at", now.to_rfc3339())
        .send()
        .await
        .map_err(s3_error)?;

    Ok((
        StatusCode::CREATED,
        Json(StoredObject {
            key,
            original_name,
            mime_type: processed.mime_type,
            size_bytes: processed.size_bytes as u64,
            kind: processed.kind.to_string(),
            owner_id,
            namespace,
            reference_id,
            created_at: now,
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

async fn object_metadata(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<StoredObject>, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = state
        .client
        .head_object()
        .bucket(&state.bucket)
        .key(&key)
        .send()
        .await
        .map_err(s3_error)?;
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
) -> Result<Response, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let object = state
        .client
        .get_object()
        .bucket(&state.bucket)
        .key(&key)
        .send()
        .await
        .map_err(s3_error)?;
    let mime_type = object
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let original_name = object
        .metadata()
        .and_then(|metadata| metadata.get("original-name"))
        .cloned()
        .unwrap_or_else(|| key.rsplit('/').next().unwrap_or("download").to_string());
    let bytes = object.body.collect().await.map_err(s3_error)?.into_bytes();
    let mut response = (StatusCode::OK, bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        download_disposition(&original_name, &mime_type),
    );
    Ok(response)
}

async fn delete_object(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(query): Query<DeleteObjectQuery>,
) -> Result<StatusCode, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = state
        .client
        .head_object()
        .bucket(&state.bucket)
        .key(&key)
        .send()
        .await
        .map_err(s3_error)?;
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
    state
        .client
        .delete_object()
        .bucket(&state.bucket)
        .key(key)
        .send()
        .await
        .map_err(s3_error)?;
    Ok(StatusCode::NO_CONTENT)
}

struct ProcessedUpload {
    bytes: Vec<u8>,
    mime_type: String,
    extension: &'static str,
    kind: &'static str,
    size_bytes: usize,
}

fn process_upload(
    bytes: &[u8],
    mime_type: &str,
    filename: &str,
) -> Result<ProcessedUpload, (StatusCode, String)> {
    if is_image(mime_type) {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Ukuran gambar maksimal 5 MB".to_string(),
            ));
        }
        let image = image::load_from_memory(bytes).map_err(|_| {
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "File gambar tidak valid".to_string(),
            )
        })?;
        let (width, height) = image.dimensions();
        if width.saturating_mul(height) > MAX_IMAGE_PIXELS {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Resolusi gambar terlalu besar".to_string(),
            ));
        }
        let candidate = if image.color().has_alpha() {
            Encoder::from_rgba(image.to_rgba8().as_raw(), width, height)
                .encode(80.0)
                .to_vec()
        } else {
            Encoder::from_rgb(image.to_rgb8().as_raw(), width, height)
                .encode(80.0)
                .to_vec()
        };
        if candidate.len() < bytes.len() {
            return Ok(ProcessedUpload {
                size_bytes: candidate.len(),
                bytes: candidate,
                mime_type: "image/webp".to_string(),
                extension: "webp",
                kind: "image",
            });
        }
        return Ok(ProcessedUpload {
            size_bytes: bytes.len(),
            bytes: bytes.to_vec(),
            mime_type: mime_type.to_string(),
            extension: extension_for(filename, mime_type),
            kind: "image",
        });
    }
    if !is_document(mime_type) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Jenis file tidak didukung".to_string(),
        ));
    }
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Ukuran dokumen maksimal 50 MB".to_string(),
        ));
    }
    Ok(ProcessedUpload {
        size_bytes: bytes.len(),
        bytes: bytes.to_vec(),
        mime_type: mime_type.to_string(),
        extension: extension_for(filename, mime_type),
        kind: "document",
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

/// Keep the S3 key opaque while giving the browser the original upload name.
/// RFC 5987's `filename*` keeps Unicode names valid in a response header.
/// Images are served `inline` so browsers and email clients render them in
/// place (Gmail/Outlook refuse to display `attachment` images); non-image
/// documents keep `attachment` so they still download instead of rendering.
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
    let kind = if is_image(mime) { "inline" } else { "attachment" };
    HeaderValue::from_str(&format!("{kind}; filename*=UTF-8''{encoded}"))
        .unwrap_or_else(|_| HeaderValue::from_str(&kind).unwrap())
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
        kind: metadata
            .get("kind")
            .cloned()
            .unwrap_or_else(|| "document".to_string()),
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

    #[test]
    fn image_conversion_never_increases_the_stored_file_size() {
        let image = ImageBuffer::from_pixel(320, 240, Rgb([32u8, 83, 158]));
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&image)
            .unwrap();

        let stored = process_upload(&jpeg, "image/jpeg", "inventory.jpg").unwrap();
        assert!(stored.size_bytes <= jpeg.len());
        if stored.mime_type == "image/webp" {
            assert!(stored.size_bytes < jpeg.len());
        }
    }
}
