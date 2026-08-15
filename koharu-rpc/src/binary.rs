//! Byte-oriented reads.
//!
//! - `GET /scene.bin` — postcard-encoded `Snapshot { epoch, scene }` (native clients).
//! - `GET /scene.json` — JSON-encoded `{ epoch, scene }` (web/UI clients).
//! - `GET /blobs/:hash` — raw blob bytes.

use std::io::{self, Write};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use image::{DynamicImage, GenericImageView, imageops::FilterType};
use koharu_core::{BlobRef, ImageRole, NodeKind, PageId, Scene};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::error::{ApiError, ApiResult};

// A correct scene should serialize to nowhere near this. This defensive
// ceiling prevents a single HTTP response from becoming large enough to
// crash the process on Windows — the real hard limit there is a single
// write() call maxing out at `u32::MAX` bytes (~4.29 GiB); this stays
// comfortably under that with headroom to spare, rather than picking an
// arbitrarily small "should be enough" number that ends up rejecting
// legitimately large projects (many-hundred-page documents).
const MAX_SCENE_JSON_BYTES: usize = 3 * 1024 * 1024 * 1024; // 3 GiB

/// Per-page size floor worth logging when hunting for the culprit — well
/// below `MAX_SCENE_JSON_BYTES` since any *one* page this size is already
/// abnormal even if the whole scene were still under the ceiling.
const NOTEWORTHY_PAGE_BYTES: usize = 1_000_000; // 1 MB

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::default()
        .routes(routes!(get_scene_bin))
        .routes(routes!(get_scene_json))
        .routes(routes!(get_blob))
        .routes(routes!(get_page_thumbnail))
}

/// JSON-shaped scene snapshot for the UI (no postcard decoder in JS).
#[derive(Serialize, utoipa::ToSchema)]
pub struct SceneSnapshot {
    pub epoch: u64,
    pub scene: Scene,
}

#[utoipa::path(
    get,
    path = "/scene.json",
    responses((status = 200, body = SceneSnapshot))
)]
async fn get_scene_json(State(app): State<AppState>) -> ApiResult<Response> {
    let session = app
        .current_session()
        .ok_or_else(|| ApiError::bad_request("no project open"))?;
    let epoch = session.epoch();
    let scene = session.scene.read();
    let page_count = scene.pages.len();

    // Serialize the in-memory scene by reference. The writer enforces the
    // ceiling while serde is producing bytes, so an oversized scene never
    // becomes a complete, unbounded Vec before it is rejected.
    let bytes = match serialize_scene_json(
        &WireSnapshot {
            epoch,
            scene: &scene,
        },
        MAX_SCENE_JSON_BYTES,
    ) {
        Ok(bytes) => bytes,
        Err(SceneJsonSerializationError::LimitExceeded) => {
            // Only runs on the (rare) failure path — cheap enough here even
            // though it re-serializes page-by-page, since we're already
            // about to fail the whole request anyway. This is what tells
            // us *which* page to actually go investigate.
            log_oversized_pages(&scene);
            tracing::error!(
                limit = MAX_SCENE_JSON_BYTES,
                pages = page_count,
                "scene.json exceeded the safe response limit — refusing to avoid a crash"
            );
            return Err(scene_json_limit_error(page_count));
        }
        Err(SceneJsonSerializationError::Serialize(err)) => {
            return Err(ApiError::internal(anyhow::Error::new(err)));
        }
    };

    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(resp)
}

/// Measures each page's own serialized size independently (via the same
/// limit-stopping writer, one page at a time so no single call has to hold
/// a multi-page buffer) and logs any that look abnormal.
fn log_oversized_pages(scene: &Scene) {
    for (id, page) in &scene.pages {
        // A generous per-page ceiling — big enough that hitting it always
        // means "this page", never "serialization takes pages in general".
        match serialize_scene_json(page, 64 * 1024 * 1024) {
            Ok(bytes) if bytes.len() > NOTEWORTHY_PAGE_BYTES => {
                tracing::error!(
                    page = %id,
                    bytes = bytes.len(),
                    "oversized page found while diagnosing scene.json"
                );
            }
            Ok(_) => {}
            Err(_) => {
                // Serializing further past 64 MiB for a single page — that
                // alone is diagnostic enough to log without the exact size.
                tracing::error!(
                    page = %id,
                    "page exceeds 64 MiB on its own while diagnosing scene.json"
                );
            }
        }
    }
}

#[derive(Serialize)]
struct WireSnapshot<'a> {
    epoch: u64,
    scene: &'a koharu_core::Scene,
}

#[derive(Debug)]
enum SceneJsonSerializationError {
    LimitExceeded,
    Serialize(serde_json::Error),
}

/// A `Write` sink that never retains more than `limit` bytes. serde_json
/// propagates its I/O error immediately, stopping serialization at the first
/// write that would exceed the configured response ceiling.
struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other(
                "scene JSON response exceeds configured limit",
            ));
        }
        self.bytes
            .try_reserve_exact(buf.len())
            .map_err(|err| io::Error::other(err.to_string()))?;
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_scene_json<T: Serialize>(
    value: &T,
    limit: usize,
) -> Result<Vec<u8>, SceneJsonSerializationError> {
    let mut writer = LimitedWriter::new(limit);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(writer.into_bytes()),
        Err(_) if writer.exceeded => Err(SceneJsonSerializationError::LimitExceeded),
        Err(err) => Err(SceneJsonSerializationError::Serialize(err)),
    }
}

fn scene_json_limit_error(page_count: usize) -> ApiError {
    ApiError::internal(anyhow::anyhow!(
        "scene.json exceeds the {} byte limit across {} pages — refusing to avoid crashing the app; \
         see the `oversized page` error(s) logged just above for which page(s) to investigate",
        MAX_SCENE_JSON_BYTES,
        page_count
    ))
}

#[utoipa::path(
    get,
    path = "/scene.bin",
    responses((status = 200, content_type = "application/octet-stream"))
)]
async fn get_scene_bin(State(app): State<AppState>) -> ApiResult<Response> {
    let session = app
        .current_session()
        .ok_or_else(|| ApiError::bad_request("no project open"))?;
    let (epoch, bytes) = {
        let scene = session.scene.read();
        let epoch = session.epoch();
        let bytes = postcard::to_allocvec(&WireSnapshot {
            epoch,
            scene: &scene,
        })
            .map_err(|e| ApiError::internal(anyhow::Error::new(e)))?;
        (epoch, bytes)
    };
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    resp.headers_mut().insert(
        "x-koharu-epoch",
        HeaderValue::from_str(&epoch.to_string())
            .map_err(|e| ApiError::internal(anyhow::Error::new(e)))?,
    );
    Ok(resp)
}

#[utoipa::path(
    get,
    path = "/blobs/{hash}",
    params(("hash" = String, Path, description = "Blake3 hash of the blob")),
    responses((status = 200, content_type = "application/octet-stream"))
)]
async fn get_blob(State(app): State<AppState>, Path(hash): Path<String>) -> ApiResult<Response> {
    let session = app
        .current_session()
        .ok_or_else(|| ApiError::bad_request("no project open"))?;
    let blob_ref = BlobRef::new(hash);
    let bytes = session
        .blobs
        .get_bytes(&blob_ref)
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "blob not found"))?;
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    Ok(resp.into_response())
}

/// Thumbnail of a page's source image. Cached on disk under
/// `.khrproj/cache/thumbs/<page_id>.webp`; generated on first request.
const THUMB_MAX_DIM: u32 = 320;

#[utoipa::path(
    get,
    path = "/pages/{id}/thumbnail",
    params(("id" = PageId, Path, description = "Page id")),
    responses((status = 200, content_type = "image/webp"))
)]
async fn get_page_thumbnail(
    State(app): State<AppState>,
    Path(id): Path<PageId>,
) -> ApiResult<Response> {
    let session = app
        .current_session()
        .ok_or_else(|| ApiError::bad_request("no project open"))?;

    // Fast path: cached file on disk.
    let thumbs_dir = session.dir.join("cache").join("thumbs");
    let cache_path = thumbs_dir.join(format!("{id}.webp"));
    if cache_path.exists()
        && let Ok(bytes) = std::fs::read(cache_path.as_std_path())
    {
        return Ok(webp_response(bytes));
    }

    // Slow path: load the page's Source image, downscale, encode, cache.
    let source_ref = {
        let scene = session.scene.read();
        let page = scene
            .page(id)
            .ok_or_else(|| ApiError::not_found(format!("page {id}")))?;
        page.nodes
            .values()
            .find_map(|n| match &n.kind {
                NodeKind::Image(img) if img.role == ImageRole::Source => Some(img.blob.clone()),
                _ => None,
            })
            .ok_or_else(|| ApiError::not_found("page has no source image"))?
    };
    let source: DynamicImage = session
        .blobs
        .load_image(&source_ref)
        .map_err(ApiError::internal)?;
    let (w, h) = source.dimensions();
    let scale = THUMB_MAX_DIM as f32 / w.max(h) as f32;
    let resized = if scale < 1.0 {
        let nw = (w as f32 * scale).round().max(1.0) as u32;
        let nh = (h as f32 * scale).round().max(1.0) as u32;
        source.resize(nw, nh, FilterType::Triangle)
    } else {
        source
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    resized
        .write_to(&mut buf, image::ImageFormat::WebP)
        .map_err(|e| ApiError::internal(anyhow::Error::new(e)))?;
    let bytes = buf.into_inner();
    let _ = std::fs::create_dir_all(thumbs_dir.as_std_path());
    let _ = std::fs::write(cache_path.as_std_path(), &bytes);
    Ok(webp_response(bytes))
}

fn webp_response(bytes: Vec<u8>) -> Response {
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("image/webp"));
    resp.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn scene_json_below_limit_preserves_snapshot_shape() {
        let scene = Scene::default();
        let bytes = serialize_scene_json(
            &WireSnapshot {
                epoch: 42,
                scene: &scene,
            },
            MAX_SCENE_JSON_BYTES,
        )
            .expect("default scene must fit under the response limit");

        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(value["epoch"], 42);
        assert_eq!(value["scene"]["project"]["name"], "");
        assert_eq!(value["scene"]["pages"], serde_json::json!({}));
    }

    #[test]
    fn scene_json_stops_when_the_limited_writer_is_full() {
        let scene = Scene::default();
        let result = serialize_scene_json(
            &WireSnapshot {
                epoch: 1,
                scene: &scene,
            },
            8,
        );

        assert!(matches!(
            result,
            Err(SceneJsonSerializationError::LimitExceeded)
        ));
    }

    #[test]
    fn limited_writer_does_not_retain_bytes_past_its_limit() {
        let mut writer = LimitedWriter::new(3);
        writer.write_all(b"abc").expect("bytes within limit");
        assert!(writer.write_all(b"d").is_err());
        assert_eq!(writer.bytes, b"abc");
    }

    #[test]
    fn scene_json_limit_failure_is_a_controlled_http_error() {
        let response = scene_json_limit_error(837).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            response.headers()[CONTENT_TYPE]
                .to_str()
                .expect("content type is ASCII")
                .contains("application/json")
        );
    }
}