//! A loaded project. One `ProjectSession` = one `.khrproj/` directory.
//!
//! Holds:
//!   - an exclusive `.lock` via `fs4` (refuses second opener)
//!   - the in-memory `Scene` behind a `parking_lot::RwLock` (never held across `.await`)
//!   - the `History` behind a `Mutex` (linear, all writes serialized)
//!   - the `BlobStore` (content-addressed images)
//!
//! On-disk layout:
//!   `.khrproj/project.toml`    — TOML-encoded `ProjectMeta`
//!   `.khrproj/scene.bin`       — postcard-encoded `Snapshot { epoch, scene }`
//!   `.khrproj/history.log`     — append-only `LogFrame { epoch, op }`
//!   `.khrproj/blobs/ab/cdef…`  — content-addressed blobs
//!   `.khrproj/.lock`           — fs4 exclusive lock (session lifetime)

use std::fs::File;
use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use atomicwrites::{AtomicFile, OverwriteBehavior};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::Utc;
use fs4::FileExt;
use koharu_core::{Scene, op::Op};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::blobs::BlobStore;
use crate::history::{self, History};
use crate::persistence;

const SCENE_FILE: &str = "scene.bin";
const LOG_FILE: &str = "history.log";
const LOCK_FILE: &str = ".lock";
const BLOBS_DIR: &str = "blobs";
const CACHE_DIR: &str = "cache";
const PROJECT_TOML: &str = "project.toml";

/// A loaded project.
pub struct ProjectSession {
    pub dir: Utf8PathBuf,
    pub scene: RwLock<Scene>,
    pub history: Mutex<History>,
    pub blobs: Arc<BlobStore>,
    /// Held for the lifetime of the session.
    _lock: File,
}

impl ProjectSession {
    /// Open an existing `.khrproj/` directory.
    pub fn open(dir: impl AsRef<Utf8Path>) -> Result<Arc<Self>> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.is_dir() {
            anyhow::bail!("not a project directory: {dir}");
        }
        Self::open_inner(dir, false)
    }

    /// Create a fresh `.khrproj/` at `dir`, failing if it already exists.
    pub fn create(dir: impl AsRef<Utf8Path>, name: impl Into<String>) -> Result<Arc<Self>> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(dir.as_std_path())
            .with_context(|| format!("create project dir {dir}"))?;
        // Project should be empty.
        let is_empty = std::fs::read_dir(dir.as_std_path())?.next().is_none();
        if !is_empty {
            anyhow::bail!("project directory not empty: {dir}");
        }
        // Seed the TOML with the name so open_inner can load it.
        let meta = ProjectTomlFile {
            name: name.into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        std::fs::write(
            dir.join(PROJECT_TOML).as_std_path(),
            toml::to_string_pretty(&meta)?,
        )?;
        Self::open_inner(dir, true)
    }

    fn open_inner(dir: Utf8PathBuf, creating: bool) -> Result<Arc<Self>> {
        std::fs::create_dir_all(dir.join(BLOBS_DIR).as_std_path())?;
        std::fs::create_dir_all(dir.join(CACHE_DIR).as_std_path())?;

        // Exclusive lock — one opener at a time.
        let lock_path = dir.join(LOCK_FILE);
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path.as_std_path())
            .with_context(|| format!("open lock file {}", lock_path))?;
        FileExt::try_lock(&lock).context("project is already open elsewhere")?;

        let blobs = Arc::new(BlobStore::open(dir.join(BLOBS_DIR).as_std_path())?);

        // Load or synthesize the scene + epoch.
        let (mut scene, mut epoch) = load_snapshot(&dir, creating)?;
        // Replay any log frames past the snapshot epoch.
        let log_path = dir.join(LOG_FILE);
        epoch = history::replay(log_path.as_std_path(), epoch, &mut scene)
            .with_context(|| format!("replay log {}", log_path))?;

        let history_obj = History::open(log_path.as_std_path(), epoch)?;

        Ok(Arc::new(Self {
            dir,
            scene: RwLock::new(scene),
            history: Mutex::new(history_obj),
            blobs,
            _lock: lock,
        }))
    }

    // --- scene mutation ----------------------------------------------------

    /// Apply an Op. Returns the epoch after apply.
    pub fn apply(&self, op: Op) -> Result<u64> {
        let mut history = self.history.lock();
        let mut scene = self.scene.write();
        history.apply(&mut scene, op)
    }

    pub fn undo(&self) -> Result<Option<(u64, Op)>> {
        let mut history = self.history.lock();
        let mut scene = self.scene.write();
        history.undo(&mut scene)
    }

    pub fn redo(&self) -> Result<Option<(u64, Op)>> {
        let mut history = self.history.lock();
        let mut scene = self.scene.write();
        history.redo(&mut scene)
    }

    pub fn epoch(&self) -> u64 {
        self.history.lock().epoch()
    }

    /// Cheap clone of the scene for read-only consumers (pipeline engines).
    pub fn scene_snapshot(&self) -> Scene {
        self.scene.read().clone()
    }

    // --- compaction --------------------------------------------------------

    /// Write a new snapshot (scene.bin) and truncate the log. Safe to call
    /// at any time; crash mid-compaction leaves the old snapshot + full log.
    pub fn compact(&self) -> Result<()> {
        let snap = {
            let scene = self.scene.read();
            let epoch = self.history.lock().epoch();
            (epoch, scene.clone())
        };
        let bytes = persistence::encode_snapshot(snap.0, &snap.1).context("encode v1 snapshot")?;

        // A correct snapshot should stay well under this even for very large
        // projects (blob bytes live under `blobs/`, not inline here) — this
        // is purely a defensive ceiling. Windows' `WriteFile` can't take a
        // single buffer over `u32::MAX` bytes at all (~4.29 GiB; the OS call
        // itself fails), and without this check that turns into a hard
        // process abort via `write_all`'s internal assertion — which, since
        // `compact()` runs unattended from the autosave loop, would then
        // repeat every ~30s and make the app effectively unusable. Stays
        // comfortably under that real ceiling rather than an arbitrarily
        // small number that ends up rejecting legitimately large projects.
        const MAX_SNAPSHOT_BYTES: usize = 3 * 1024 * 1024 * 1024; // 3 GiB
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            for (id, page) in &snap.1.pages {
                if let Ok(page_bytes) = postcard::to_allocvec(page)
                    && page_bytes.len() > 1_000_000
                {
                    tracing::error!(
                        page = %id,
                        bytes = page_bytes.len(),
                        "oversized page found while diagnosing an oversized scene snapshot"
                    );
                }
            }
            anyhow::bail!(
                "scene snapshot is {} bytes (limit {} bytes) — refusing to write to avoid a \
                 hard crash; see the `oversized page` error(s) logged just above for which \
                 page(s) to investigate",
                bytes.len(),
                MAX_SNAPSHOT_BYTES
            );
        }

        AtomicFile::new(
            self.dir.join(SCENE_FILE).as_std_path(),
            OverwriteBehavior::AllowOverwrite,
        )
            .write(|f| f.write_all(&bytes))
            .context("write scene.bin atomically")?;
        // Log truncation only after snapshot is durably on disk.
        self.history.lock().truncate_log()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Snapshot loading / TOML metadata
// ---------------------------------------------------------------------------

fn load_snapshot(dir: &Utf8Path, creating: bool) -> Result<(Scene, u64)> {
    let scene_path = dir.join(SCENE_FILE);
    if scene_path.exists() {
        let bytes = std::fs::read(scene_path.as_std_path())
            .with_context(|| format!("read {}", scene_path))?;
        let (scene, epoch, format) = persistence::decode_snapshot(&bytes)
            .with_context(|| format!("decode {}", scene_path))?;
        if format == persistence::Format::LegacyV0 {
            tracing::info!(path = %scene_path, "opened legacy v0 scene snapshot; it will upgrade on next compact");
        }
        return Ok((scene, epoch));
    }

    // No snapshot — build one from `project.toml` (or defaults for creation).
    let toml_path = dir.join(PROJECT_TOML);
    let meta = if toml_path.exists() {
        let text = std::fs::read_to_string(toml_path.as_std_path())?;
        toml::from_str::<ProjectTomlFile>(&text).with_context(|| format!("parse {}", toml_path))?
    } else if creating {
        ProjectTomlFile {
            name: String::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    } else {
        anyhow::bail!("missing project.toml at {}", toml_path);
    };

    let mut scene = Scene::default();
    scene.project.name = meta.name;
    scene.project.created_at = meta.created_at;
    scene.project.updated_at = meta.updated_at;
    Ok((scene, 0))
}

#[derive(Serialize, Deserialize)]
struct ProjectTomlFile {
    name: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use indexmap::IndexMap;
    use koharu_core::{
        Node, NodeDataPatch, NodeId, NodeKind, NodePatch, Op, Page, PageId, TextData,
        TextDataPatch, TextShaderEffect, TextStyle, Transform,
    };
    use tempfile::tempdir;

    fn tmp_dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path.join("proj.khrproj"))
    }

    #[test]
    fn create_apply_close_reopen_preserves_scene() {
        let (_tmp, path) = tmp_dir();
        let page_id: PageId;
        {
            let session = ProjectSession::create(&path, "test").unwrap();
            let page = Page::new("p1", 800, 600);
            page_id = page.id;
            session
                .apply(Op::AddPage { page, at: 0 })
                .expect("apply AddPage");
            session.compact().unwrap();
            // Session drops, lock released.
        }
        let session = ProjectSession::open(&path).unwrap();
        assert_eq!(session.scene.read().pages.len(), 1);
        assert!(session.scene.read().pages.contains_key(&page_id));
    }

    #[test]
    fn reopen_preserves_text_style_effects_in_scene_bin() {
        let (_tmp, path) = tmp_dir();
        let page_id: PageId;
        let node_id: NodeId;
        {
            let session = ProjectSession::create(&path, "styled").unwrap();
            let page = Page::new("p1", 800, 600);
            page_id = page.id;
            session
                .apply(Op::AddPage { page, at: 0 })
                .expect("apply AddPage");

            node_id = NodeId::new();
            let mut scene = session.scene.write();
            let page = scene.pages.get_mut(&page_id).expect("page");
            page.nodes.insert(
                node_id,
                Node {
                    id: node_id,
                    transform: Transform {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 40.0,
                        rotation_deg: 0.0,
                    },
                    visible: true,
                    kind: NodeKind::Text(TextData {
                        style: Some(TextStyle {
                            font_families: vec!["Arial".to_string()],
                            font_size: Some(20.0),
                            color: [0, 0, 0, 255],
                            effect: Some(TextShaderEffect {
                                italic: true,
                                bold: true,
                            }),
                            stroke: None,
                            text_align: None,
                            line_spacing: Some(1.35),
                            letter_spacing: Some(2.5),
                        }),
                        ..Default::default()
                    }),
                },
            );
            drop(scene);
            session.compact().unwrap();
        }

        let session = ProjectSession::open(&path).unwrap();
        let scene = session.scene.read();
        let page = scene.pages.get(&page_id).expect("page");
        let node = page.nodes.get(&node_id).expect("node");
        let NodeKind::Text(text) = &node.kind else {
            panic!("expected text node");
        };
        let effect = text
            .style
            .as_ref()
            .and_then(|style| style.effect)
            .expect("effect");
        assert!(effect.italic);
        assert!(effect.bold);
        let style = text.style.as_ref().expect("style");
        assert_eq!(style.line_spacing, Some(1.35));
        assert_eq!(style.letter_spacing, Some(2.5));
    }

    #[test]
    fn exclusive_lock_prevents_second_open() {
        let (_tmp, path) = tmp_dir();
        let a = ProjectSession::create(&path, "test").unwrap();
        let err = ProjectSession::open(&path)
            .err()
            .expect("second open must fail");
        assert!(err.to_string().contains("already open"));
        drop(a);
    }

    /// Representative V0 fixture serialized through the frozen legacy schema,
    /// not the current Scene type. This is the exact unheadered postcard shape
    /// written by pre-versioning releases.
    #[test]
    fn legacy_snapshot_opens_edits_upgrades_and_reopens() {
        let (_tmp, path) = tmp_dir();
        std::fs::create_dir_all(path.as_std_path()).unwrap();
        let page_id = PageId::new();
        let node_id = NodeId::new();
        let legacy_node = crate::persistence::LegacyNodeV0 {
            id: node_id,
            transform: crate::persistence::LegacyTransformV0 {
                x: 10.0,
                y: 20.0,
                width: 120.0,
                height: 48.0,
                rotation_deg: 0.0,
            },
            visible: true,
            kind: crate::persistence::LegacyNodeKindV0::Text(
                crate::persistence::LegacyTextDataV0 {
                    confidence: 0.9,
                    source_lang: Some("ja".into()),
                    source_direction: None,
                    rendered_direction: None,
                    line_polygons: None,
                    rotation_deg: None,
                    detected_font_size_px: None,
                    detector: Some("fixture".into()),
                    text: Some("古い".into()),
                    translation: Some("legacy text".into()),
                    style: Some(crate::persistence::LegacyTextStyleV0 {
                        font_families: vec!["Arial".into()],
                        font_size: Some(20.0),
                        color: [1, 2, 3, 255],
                        effect: Some(crate::persistence::LegacyTextShaderEffectV0 {
                            italic: true,
                            bold: false,
                        }),
                        stroke: None,
                        text_align: Some(crate::persistence::LegacyTextAlignV0::Center),
                    }),
                    font_prediction: None,
                    sprite: None,
                    sprite_transform: None,
                    lock_layout_box: true,
                },
            ),
        };
        let mut nodes = IndexMap::new();
        nodes.insert(node_id, legacy_node);
        let mut pages = IndexMap::new();
        pages.insert(
            page_id,
            crate::persistence::LegacyPageV0 {
                id: page_id,
                name: "legacy-page".into(),
                width: 800,
                height: 1200,
                nodes,
            },
        );
        let fixture = crate::persistence::LegacySnapshotV0 {
            epoch: 7,
            scene: crate::persistence::LegacySceneV0 {
                project: crate::persistence::LegacyProjectMetaV0 {
                    name: "legacy-project".into(),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                    style: crate::persistence::LegacyProjectStyleV0::default(),
                },
                pages,
            },
        };
        std::fs::write(
            path.join(SCENE_FILE).as_std_path(),
            crate::persistence::encode_legacy_snapshot_for_test(&fixture),
        )
            .unwrap();

        {
            let session = ProjectSession::open(&path).unwrap();
            assert_eq!(session.epoch(), 7);
            let scene = session.scene.read();
            let NodeKind::Text(text) = &scene.pages[&page_id].nodes[&node_id].kind else {
                panic!("fixture node must be text");
            };
            assert_eq!(text.translation.as_deref(), Some("legacy text"));
            assert!(text.style.as_ref().unwrap().effect.unwrap().italic);
            drop(scene);

            session
                .apply(Op::UpdateNode {
                    page: page_id,
                    id: node_id,
                    patch: NodePatch {
                        data: Some(NodeDataPatch::Text(TextDataPatch {
                            translation: Some(Some("edited after migration".into())),
                            ..Default::default()
                        })),
                        ..Default::default()
                    },
                    prev: NodePatch::default(),
                })
                .unwrap();
            session.compact().unwrap();
        }

        let bytes = std::fs::read(path.join(SCENE_FILE).as_std_path()).unwrap();
        assert!(bytes.starts_with(b"KHRSCN\0"));
        assert_eq!(u16::from_le_bytes([bytes[7], bytes[8]]), 2);
        let reopened = ProjectSession::open(&path).unwrap();
        assert_eq!(reopened.epoch(), 8);
        let scene = reopened.scene.read();
        let NodeKind::Text(text) = &scene.pages[&page_id].nodes[&node_id].kind else {
            panic!("fixture node must be text");
        };
        assert_eq!(text.translation.as_deref(), Some("edited after migration"));
    }

    #[test]
    fn v1_snapshot_opens_edits_upgrades_and_reopens_as_v2() {
        let (_tmp, path) = tmp_dir();
        std::fs::create_dir_all(path.as_std_path()).unwrap();
        let page_id = PageId::new();
        let page = crate::persistence::LegacyPageV0 {
            id: page_id,
            name: "v1-page".into(),
            width: 640,
            height: 480,
            nodes: IndexMap::new(),
        };
        let mut pages = IndexMap::new();
        pages.insert(page_id, page);
        let fixture = crate::persistence::LegacySnapshotV0 {
            epoch: 11,
            scene: crate::persistence::LegacySceneV0 {
                project: crate::persistence::LegacyProjectMetaV0 {
                    name: "v1-project".into(),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                    style: crate::persistence::LegacyProjectStyleV0::default(),
                },
                pages,
            },
        };
        std::fs::write(
            path.join(SCENE_FILE).as_std_path(),
            crate::persistence::encode_v1_snapshot_for_test(&fixture),
        )
            .unwrap();

        {
            let session = ProjectSession::open(&path).unwrap();
            assert_eq!(session.epoch(), 11);
            assert_eq!(session.scene.read().project.name, "v1-project");
            assert!(session.scene.read().pages.contains_key(&page_id));
            session
                .apply(Op::UpdateProjectMeta {
                    patch: koharu_core::ProjectMetaPatch {
                        name: Some("edited v1".into()),
                        ..Default::default()
                    },
                    prev: Default::default(),
                })
                .unwrap();
            session.compact().unwrap();
        }

        let bytes = std::fs::read(path.join(SCENE_FILE).as_std_path()).unwrap();
        assert!(bytes.starts_with(b"KHRSCN\0"));
        assert_eq!(u16::from_le_bytes([bytes[7], bytes[8]]), 2);
        let reopened = ProjectSession::open(&path).unwrap();
        assert_eq!(reopened.epoch(), 12);
        assert_eq!(reopened.scene.read().project.name, "edited v1");
    }

    #[test]
    fn v2_snapshot_opens_normally() {
        let (_tmp, path) = tmp_dir();
        {
            let session = ProjectSession::create(&path, "v2-project").unwrap();
            session.compact().unwrap();
        }
        let bytes = std::fs::read(path.join(SCENE_FILE).as_std_path()).unwrap();
        assert!(bytes.starts_with(b"KHRSCN\0"));
        assert_eq!(u16::from_le_bytes([bytes[7], bytes[8]]), 2);
        let reopened = ProjectSession::open(&path).unwrap();
        assert_eq!(reopened.scene.read().project.name, "v2-project");
    }
}