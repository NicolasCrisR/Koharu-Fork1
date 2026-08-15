//! Versioned on-disk codecs for project snapshots and history frames.
//!
//! V0 had no header and encoded the scene/op graph directly with postcard.
//! Keep its schema here, frozen, so future changes to core types never cause
//! legacy bytes to be decoded through a changed struct layout.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use koharu_core::{
    BlobRef, FontPrediction, ImageData, ImageRole, MaskData, MaskRole, Node, NodeDataPatch, NodeId,
    NodeKind, NodePatch, Op, Page, PageId, PagePatch, ProjectMeta, ProjectMetaPatch, ProjectStyle,
    Scene, TextAlign, TextData, TextDataPatch, TextDirection, TextShaderEffect, TextStrokeStyle,
    TextStyle, Transform,
};
use serde::{Deserialize, Serialize};

const SNAPSHOT_MAGIC: &[u8; 7] = b"KHRSCN\0";
const LOG_FRAME_MAGIC: &[u8; 7] = b"KHRLOG\0";
const FORMAT_V1: u16 = 1;
const FORMAT_V2: u16 = 2;
const CURRENT_FORMAT: u16 = FORMAT_V2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    LegacyV0,
    V1,
    V2,
}

/// The V1 wire schema is deliberately independent from the live core types.
///
/// V1 originally encoded `Scene`/`Op` directly. At the time those types had
/// the exact same postcard layout as the frozen V0 graph below (the only
/// difference between V0 and V1 was the header). Keep that graph as the V1
/// decoder too: future additions to the live types must never be used to
/// decode already-written V1 bytes.
#[derive(Serialize, Deserialize)]
struct SnapshotV1Frozen {
    epoch: u64,
    scene: LegacySceneV0,
}

#[derive(Serialize, Deserialize)]
struct LogFrameV1Frozen {
    epoch: u64,
    op: LegacyOpV0,
}

#[derive(Serialize, Deserialize)]
struct SnapshotV2 {
    epoch: u64,
    scene: Scene,
}

#[derive(Serialize, Deserialize)]
struct LogFrameV2 {
    epoch: u64,
    op: Op,
}

pub(crate) fn encode_snapshot(epoch: u64, scene: &Scene) -> Result<Vec<u8>> {
    let mut out = header(SNAPSHOT_MAGIC);
    out.extend(postcard::to_allocvec(&SnapshotV2 {
        epoch,
        scene: scene.clone(),
    })?);
    Ok(out)
}

pub(crate) fn decode_snapshot(bytes: &[u8]) -> Result<(Scene, u64, Format)> {
    if let Some((version, payload)) = versioned_payload(bytes, SNAPSHOT_MAGIC)? {
        return match version {
            FORMAT_V1 => {
                let snapshot: SnapshotV1Frozen =
                    postcard::from_bytes(payload).context("decode v1 snapshot")?;
                Ok((snapshot.scene.into(), snapshot.epoch, Format::V1))
            }
            FORMAT_V2 => {
                let snapshot: SnapshotV2 =
                    postcard::from_bytes(payload).context("decode v2 snapshot")?;
                Ok((snapshot.scene, snapshot.epoch, Format::V2))
            }
            _ => bail!("unsupported persistence format version {version}"),
        };
    }
    let snapshot: LegacySnapshotV0 =
        postcard::from_bytes(bytes).context("decode legacy v0 snapshot")?;
    Ok((snapshot.scene.into(), snapshot.epoch, Format::LegacyV0))
}

pub(crate) fn encode_log_frame(epoch: u64, op: &Op) -> Result<Vec<u8>> {
    let mut out = header(LOG_FRAME_MAGIC);
    out.extend(postcard::to_allocvec(&LogFrameV2 {
        epoch,
        op: op.clone(),
    })?);
    Ok(out)
}

pub(crate) fn decode_log_frame(bytes: &[u8]) -> Result<(u64, Op, Format)> {
    if let Some((version, payload)) = versioned_payload(bytes, LOG_FRAME_MAGIC)? {
        return match version {
            FORMAT_V1 => {
                let frame: LogFrameV1Frozen =
                    postcard::from_bytes(payload).context("decode v1 history frame")?;
                Ok((frame.epoch, frame.op.into(), Format::V1))
            }
            FORMAT_V2 => {
                let frame: LogFrameV2 =
                    postcard::from_bytes(payload).context("decode v2 history frame")?;
                Ok((frame.epoch, frame.op, Format::V2))
            }
            _ => bail!("unsupported persistence format version {version}"),
        };
    }
    let frame: LegacyLogFrameV0 =
        postcard::from_bytes(bytes).context("decode legacy v0 history frame")?;
    Ok((frame.epoch, frame.op.into(), Format::LegacyV0))
}

fn header(magic: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(magic.len() + 2);
    out.extend_from_slice(magic);
    out.extend_from_slice(&CURRENT_FORMAT.to_le_bytes());
    out
}

fn versioned_payload<'a>(bytes: &'a [u8], magic: &[u8]) -> Result<Option<(u16, &'a [u8])>> {
    if !bytes.starts_with(magic) {
        return Ok(None);
    }
    if bytes.len() < magic.len() + 2 {
        bail!("truncated versioned persistence header");
    }
    let version = u16::from_le_bytes([bytes[magic.len()], bytes[magic.len() + 1]]);
    Ok(Some((version, &bytes[magic.len() + 2..])))
}

// ---------------------------------------------------------------------------
// Frozen V0 schema. Do not replace these types with current core types.
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacySnapshotV0 {
    pub epoch: u64,
    pub scene: LegacySceneV0,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyLogFrameV0 {
    pub epoch: u64,
    pub op: LegacyOpV0,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacySceneV0 {
    pub project: LegacyProjectMetaV0,
    pub pages: IndexMap<PageId, LegacyPageV0>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyProjectMetaV0 {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub style: LegacyProjectStyleV0,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LegacyProjectStyleV0 {
    pub default_font: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyPageV0 {
    pub id: PageId,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub nodes: IndexMap<NodeId, LegacyNodeV0>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyNodeV0 {
    pub id: NodeId,
    pub transform: LegacyTransformV0,
    pub visible: bool,
    pub kind: LegacyNodeKindV0,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum LegacyNodeKindV0 {
    Image(LegacyImageDataV0),
    Text(LegacyTextDataV0),
    Mask(LegacyMaskDataV0),
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyImageDataV0 {
    pub role: ImageRole,
    pub blob: BlobRef,
    pub opacity: f32,
    pub natural_width: u32,
    pub natural_height: u32,
    pub name: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyMaskDataV0 {
    pub role: MaskRole,
    pub blob: BlobRef,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyTextDataV0 {
    pub confidence: f32,
    pub source_lang: Option<String>,
    pub source_direction: Option<TextDirection>,
    pub rendered_direction: Option<TextDirection>,
    pub line_polygons: Option<Vec<[[f32; 2]; 4]>>,
    pub rotation_deg: Option<f32>,
    pub detected_font_size_px: Option<f32>,
    pub detector: Option<String>,
    pub text: Option<String>,
    pub translation: Option<String>,
    pub style: Option<LegacyTextStyleV0>,
    pub font_prediction: Option<FontPrediction>,
    pub sprite: Option<BlobRef>,
    pub sprite_transform: Option<LegacyTransformV0>,
    pub lock_layout_box: bool,
}
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct LegacyTransformV0 {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub rotation_deg: f32,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyTextStyleV0 {
    pub font_families: Vec<String>,
    pub font_size: Option<f32>,
    pub color: [u8; 4],
    pub effect: Option<LegacyTextShaderEffectV0>,
    pub stroke: Option<LegacyTextStrokeStyleV0>,
    pub text_align: Option<LegacyTextAlignV0>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct LegacyTextShaderEffectV0 {
    pub italic: bool,
    pub bold: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LegacyTextStrokeStyleV0 {
    pub enabled: bool,
    pub color: [u8; 4],
    pub width_px: Option<f32>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) enum LegacyTextAlignV0 {
    Left,
    Center,
    Right,
}

impl From<LegacySceneV0> for Scene {
    fn from(v: LegacySceneV0) -> Self {
        Self {
            project: v.project.into(),
            pages: v
                .pages
                .into_iter()
                .map(|(id, page)| (id, page.into()))
                .collect(),
        }
    }
}
impl From<LegacyProjectMetaV0> for ProjectMeta {
    fn from(v: LegacyProjectMetaV0) -> Self {
        Self {
            name: v.name,
            created_at: v.created_at,
            updated_at: v.updated_at,
            style: v.style.into(),
        }
    }
}
impl From<LegacyProjectStyleV0> for ProjectStyle {
    fn from(v: LegacyProjectStyleV0) -> Self {
        Self {
            default_font: v.default_font,
        }
    }
}
impl From<LegacyPageV0> for Page {
    fn from(v: LegacyPageV0) -> Self {
        Self {
            id: v.id,
            name: v.name,
            width: v.width,
            height: v.height,
            nodes: v
                .nodes
                .into_iter()
                .map(|(id, node)| (id, node.into()))
                .collect(),
        }
    }
}
impl From<LegacyNodeV0> for Node {
    fn from(v: LegacyNodeV0) -> Self {
        Self {
            id: v.id,
            transform: v.transform.into(),
            visible: v.visible,
            kind: v.kind.into(),
        }
    }
}
impl From<LegacyNodeKindV0> for NodeKind {
    fn from(v: LegacyNodeKindV0) -> Self {
        match v {
            LegacyNodeKindV0::Image(x) => Self::Image(x.into()),
            LegacyNodeKindV0::Text(x) => Self::Text(x.into()),
            LegacyNodeKindV0::Mask(x) => Self::Mask(x.into()),
        }
    }
}
impl From<LegacyImageDataV0> for ImageData {
    fn from(v: LegacyImageDataV0) -> Self {
        Self {
            role: v.role,
            blob: v.blob,
            opacity: v.opacity,
            natural_width: v.natural_width,
            natural_height: v.natural_height,
            name: v.name,
        }
    }
}
impl From<LegacyMaskDataV0> for MaskData {
    fn from(v: LegacyMaskDataV0) -> Self {
        Self {
            role: v.role,
            blob: v.blob,
        }
    }
}
impl From<LegacyTextDataV0> for TextData {
    fn from(v: LegacyTextDataV0) -> Self {
        Self {
            confidence: v.confidence,
            source_lang: v.source_lang,
            source_direction: v.source_direction,
            rendered_direction: v.rendered_direction,
            line_polygons: v.line_polygons,
            rotation_deg: v.rotation_deg,
            detected_font_size_px: v.detected_font_size_px,
            detector: v.detector,
            text: v.text,
            translation: v.translation,
            style: v.style.map(Into::into),
            font_prediction: v.font_prediction,
            sprite: v.sprite,
            sprite_transform: v.sprite_transform.map(Into::into),
            lock_layout_box: v.lock_layout_box,
        }
    }
}
impl From<LegacyTransformV0> for Transform {
    fn from(v: LegacyTransformV0) -> Self {
        Self {
            x: v.x,
            y: v.y,
            width: v.width,
            height: v.height,
            rotation_deg: v.rotation_deg,
        }
    }
}
impl From<LegacyTextStyleV0> for TextStyle {
    fn from(v: LegacyTextStyleV0) -> Self {
        Self {
            font_families: v.font_families,
            font_size: v.font_size,
            color: v.color,
            effect: v.effect.map(Into::into),
            stroke: v.stroke.map(Into::into),
            text_align: v.text_align.map(Into::into),
            line_spacing: None,
            letter_spacing: None,
        }
    }
}
impl From<LegacyTextShaderEffectV0> for TextShaderEffect {
    fn from(v: LegacyTextShaderEffectV0) -> Self {
        Self {
            italic: v.italic,
            bold: v.bold,
        }
    }
}
impl From<LegacyTextStrokeStyleV0> for TextStrokeStyle {
    fn from(v: LegacyTextStrokeStyleV0) -> Self {
        Self {
            enabled: v.enabled,
            color: v.color,
            width_px: v.width_px,
        }
    }
}
impl From<LegacyTextAlignV0> for TextAlign {
    fn from(v: LegacyTextAlignV0) -> Self {
        match v {
            LegacyTextAlignV0::Left => Self::Left,
            LegacyTextAlignV0::Center => Self::Center,
            LegacyTextAlignV0::Right => Self::Right,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum LegacyOpV0 {
    UpdateProjectMeta {
        patch: LegacyProjectMetaPatchV0,
        prev: LegacyProjectMetaPatchV0,
    },
    AddPage {
        page: LegacyPageV0,
        at: usize,
    },
    RemovePage {
        id: PageId,
        prev_page: LegacyPageV0,
        prev_index: usize,
    },
    UpdatePage {
        id: PageId,
        patch: LegacyPagePatchV0,
        prev: LegacyPagePatchV0,
    },
    ReorderPages {
        order: Vec<PageId>,
        prev_order: Vec<PageId>,
    },
    AddNode {
        page: PageId,
        node: LegacyNodeV0,
        at: usize,
    },
    RemoveNode {
        page: PageId,
        id: NodeId,
        prev_node: LegacyNodeV0,
        prev_index: usize,
    },
    UpdateNode {
        page: PageId,
        id: NodeId,
        patch: LegacyNodePatchV0,
        prev: LegacyNodePatchV0,
    },
    ReorderNodes {
        page: PageId,
        order: Vec<NodeId>,
        prev_order: Vec<NodeId>,
    },
    Batch {
        ops: Vec<LegacyOpV0>,
        label: String,
    },
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LegacyProjectMetaPatchV0 {
    pub name: Option<String>,
    pub style: Option<LegacyProjectStyleV0>,
    pub updated_at: Option<DateTime<Utc>>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LegacyPagePatchV0 {
    pub name: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LegacyNodePatchV0 {
    pub transform: Option<LegacyTransformV0>,
    pub visible: Option<bool>,
    pub data: Option<LegacyNodeDataPatchV0>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum LegacyNodeDataPatchV0 {
    Text(LegacyTextDataPatchV0),
    Image(LegacyImageDataPatchV0),
    Mask(LegacyMaskDataPatchV0),
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LegacyTextDataPatchV0 {
    pub confidence: Option<f32>,
    pub source_lang: Option<Option<String>>,
    pub source_direction: Option<Option<TextDirection>>,
    pub rendered_direction: Option<Option<TextDirection>>,
    pub line_polygons: Option<Option<Vec<[[f32; 2]; 4]>>>,
    pub rotation_deg: Option<Option<f32>>,
    pub detected_font_size_px: Option<Option<f32>>,
    pub detector: Option<Option<String>>,
    pub text: Option<Option<String>>,
    pub translation: Option<Option<String>>,
    pub style: Option<Option<LegacyTextStyleV0>>,
    pub font_prediction: Option<Option<FontPrediction>>,
    pub sprite: Option<Option<BlobRef>>,
    pub sprite_transform: Option<Option<LegacyTransformV0>>,
    pub lock_layout_box: Option<bool>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LegacyImageDataPatchV0 {
    pub blob: Option<BlobRef>,
    pub opacity: Option<f32>,
    pub name: Option<Option<String>>,
    pub natural_width: Option<u32>,
    pub natural_height: Option<u32>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LegacyMaskDataPatchV0 {
    pub blob: Option<BlobRef>,
}

impl From<LegacyOpV0> for Op {
    fn from(v: LegacyOpV0) -> Self {
        match v {
            LegacyOpV0::UpdateProjectMeta { patch, prev } => Self::UpdateProjectMeta {
                patch: patch.into(),
                prev: prev.into(),
            },
            LegacyOpV0::AddPage { page, at } => Self::AddPage {
                page: page.into(),
                at,
            },
            LegacyOpV0::RemovePage {
                id,
                prev_page,
                prev_index,
            } => Self::RemovePage {
                id,
                prev_page: prev_page.into(),
                prev_index,
            },
            LegacyOpV0::UpdatePage { id, patch, prev } => Self::UpdatePage {
                id,
                patch: patch.into(),
                prev: prev.into(),
            },
            LegacyOpV0::ReorderPages { order, prev_order } => {
                Self::ReorderPages { order, prev_order }
            }
            LegacyOpV0::AddNode { page, node, at } => Self::AddNode {
                page,
                node: node.into(),
                at,
            },
            LegacyOpV0::RemoveNode {
                page,
                id,
                prev_node,
                prev_index,
            } => Self::RemoveNode {
                page,
                id,
                prev_node: prev_node.into(),
                prev_index,
            },
            LegacyOpV0::UpdateNode {
                page,
                id,
                patch,
                prev,
            } => Self::UpdateNode {
                page,
                id,
                patch: patch.into(),
                prev: prev.into(),
            },
            LegacyOpV0::ReorderNodes {
                page,
                order,
                prev_order,
            } => Self::ReorderNodes {
                page,
                order,
                prev_order,
            },
            LegacyOpV0::Batch { ops, label } => Self::Batch {
                ops: ops.into_iter().map(Into::into).collect(),
                label,
            },
        }
    }
}
impl From<LegacyProjectMetaPatchV0> for ProjectMetaPatch {
    fn from(v: LegacyProjectMetaPatchV0) -> Self {
        Self {
            name: v.name,
            style: v.style.map(Into::into),
            updated_at: v.updated_at,
        }
    }
}
impl From<LegacyPagePatchV0> for PagePatch {
    fn from(v: LegacyPagePatchV0) -> Self {
        Self {
            name: v.name,
            width: v.width,
            height: v.height,
        }
    }
}
impl From<LegacyNodePatchV0> for NodePatch {
    fn from(v: LegacyNodePatchV0) -> Self {
        Self {
            transform: v.transform.map(Into::into),
            visible: v.visible,
            data: v.data.map(Into::into),
        }
    }
}
impl From<LegacyNodeDataPatchV0> for NodeDataPatch {
    fn from(v: LegacyNodeDataPatchV0) -> Self {
        match v {
            LegacyNodeDataPatchV0::Text(x) => Self::Text(x.into()),
            LegacyNodeDataPatchV0::Image(x) => Self::Image(x.into()),
            LegacyNodeDataPatchV0::Mask(x) => Self::Mask(x.into()),
        }
    }
}
impl From<LegacyTextDataPatchV0> for TextDataPatch {
    fn from(v: LegacyTextDataPatchV0) -> Self {
        Self {
            confidence: v.confidence,
            source_lang: v.source_lang,
            source_direction: v.source_direction,
            rendered_direction: v.rendered_direction,
            line_polygons: v.line_polygons,
            rotation_deg: v.rotation_deg,
            detected_font_size_px: v.detected_font_size_px,
            detector: v.detector,
            text: v.text,
            translation: v.translation,
            style: v.style.map(|x| x.map(Into::into)),
            font_prediction: v.font_prediction,
            sprite: v.sprite,
            sprite_transform: v.sprite_transform.map(|x| x.map(Into::into)),
            lock_layout_box: v.lock_layout_box,
        }
    }
}
impl From<LegacyImageDataPatchV0> for koharu_core::ImageDataPatch {
    fn from(v: LegacyImageDataPatchV0) -> Self {
        Self {
            blob: v.blob,
            opacity: v.opacity,
            name: v.name,
            natural_width: v.natural_width,
            natural_height: v.natural_height,
        }
    }
}
impl From<LegacyMaskDataPatchV0> for koharu_core::MaskDataPatch {
    fn from(v: LegacyMaskDataPatchV0) -> Self {
        Self { blob: v.blob }
    }
}

#[cfg(test)]
pub(crate) fn encode_legacy_snapshot_for_test(snapshot: &LegacySnapshotV0) -> Vec<u8> {
    postcard::to_allocvec(snapshot).unwrap()
}

#[cfg(test)]
pub(crate) fn encode_legacy_log_frame_for_test(frame: &LegacyLogFrameV0) -> Vec<u8> {
    postcard::to_allocvec(frame).unwrap()
}

/// Encodes an authentic V1 frame through the frozen V1 schema. This is only
/// for regression fixtures: production writes always use the current V2
/// encoder above.
#[cfg(test)]
pub(crate) fn encode_v1_snapshot_for_test(snapshot: &LegacySnapshotV0) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(&FORMAT_V1.to_le_bytes());
    out.extend(
        postcard::to_allocvec(&SnapshotV1Frozen {
            epoch: snapshot.epoch,
            scene: snapshot.scene.clone(),
        })
        .unwrap(),
    );
    out
}

#[cfg(test)]
pub(crate) fn encode_v1_log_frame_for_test(frame: &LegacyLogFrameV0) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(LOG_FRAME_MAGIC);
    out.extend_from_slice(&FORMAT_V1.to_le_bytes());
    out.extend(
        postcard::to_allocvec(&LogFrameV1Frozen {
            epoch: frame.epoch,
            op: frame.op.clone(),
        })
        .unwrap(),
    );
    out
}
