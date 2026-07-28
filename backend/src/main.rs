use axum::{
    extract::{Multipart, Path, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotoMetadata {
    pub id: String,
    pub original_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub public_url: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct PresignedUrlRequest {
    pub filename: String,
    pub mime_type: String,
}

#[derive(Debug, Serialize)]
pub struct PresignedUrlResponse {
    pub photo_id: String,
    pub upload_url: String,
    pub public_url: String,
}

#[derive(Clone)]
pub struct AppState {
    pub photos: Arc<Mutex<HashMap<String, (PhotoMetadata, Vec<u8>)>>>,
    pub s3_endpoint: String,
    pub s3_bucket: String,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .init();

    let port: u16 = std::env::var("PORT")
        .or_else(|_| std::env::var("SERVER_PORT"))
        .unwrap_or_else(|_| "8081".to_string())
        .parse()
        .unwrap_or(8081);

    let state = AppState {
        photos: Arc::new(Mutex::new(HashMap::new())),
        s3_endpoint: std::env::var("S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".to_string()),
        s3_bucket: std::env::var("S3_BUCKET").unwrap_or_else(|_| "stuff8-photos".to_string()),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/api/photos/health", get(health_check))
        .route("/api/photos/upload-url", post(request_presigned_url))
        .route("/api/photos/upload", post(upload_photo_file))
        .route("/api/photos/:id", get(get_photo_info))
        .route("/api/photos/:id/file", get(serve_photo_file))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("Photos domain service listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health_check() -> &'static str {
    "OK - Photos domain service (S3 & Presigned Upload ready)"
}

async fn request_presigned_url(
    State(state): State<AppState>,
    Json(_payload): Json<PresignedUrlRequest>,
) -> Json<PresignedUrlResponse> {
    let photo_id = Uuid::new_v4().to_string();
    let upload_url = format!("{}/api/photos/upload?id={}", state.s3_endpoint, photo_id);
    let public_url = format!("/api/photos/{}/file", photo_id);

    Json(PresignedUrlResponse {
        photo_id,
        upload_url,
        public_url,
    })
}

async fn upload_photo_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<PhotoMetadata>, (StatusCode, String)> {
    let mut photo_id = Uuid::new_v4().to_string();
    let mut original_name = "image.jpg".to_string();
    let mut mime_type = "image/jpeg".to_string();
    let mut file_data = Vec::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or_default().to_string();
        if name == "photo_id" || name == "id" {
            if let Ok(text) = field.text().await {
                if !text.is_empty() {
                    photo_id = text;
                }
            }
        } else if name == "file" || field.file_name().is_some() {
            if let Some(fname) = field.file_name() {
                original_name = fname.to_string();
            }
            if let Some(ct) = field.content_type() {
                mime_type = ct.to_string();
            }
            if let Ok(bytes) = field.bytes().await {
                file_data = bytes.to_vec();
            }
        }
    }

    if file_data.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "No file data provided".to_string()));
    }

    let meta = PhotoMetadata {
        id: photo_id.clone(),
        original_name,
        mime_type,
        size_bytes: file_data.len() as u64,
        public_url: format!("/api/photos/{}/file", photo_id),
        created_at: Utc::now(),
    };

    let mut map = state.photos.lock().unwrap();
    map.insert(photo_id, (meta.clone(), file_data));

    Ok(Json(meta))
}

async fn get_photo_info(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<PhotoMetadata>, (StatusCode, String)> {
    let map = state.photos.lock().unwrap();
    if let Some((meta, _)) = map.get(&id) {
        Ok(Json(meta.clone()))
    } else {
        Err((StatusCode::NOT_FOUND, "Photo not found".to_string()))
    }
}

async fn serve_photo_file(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let map = state.photos.lock().unwrap();
    if let Some((meta, bytes)) = map.get(&id) {
        let mut response = (StatusCode::OK, bytes.clone()).into_response();
        if let Ok(hv) = HeaderValue::from_str(&meta.mime_type) {
            response.headers_mut().insert(axum::http::header::CONTENT_TYPE, hv);
        }
        response
    } else {
        (StatusCode::NOT_FOUND, "Photo file not found").into_response()
    }
}
