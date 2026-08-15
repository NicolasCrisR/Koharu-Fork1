//! "Preencher → Detectar" — fills the segmentation mask with a solid color
//! sampled from the pixels immediately surrounding it, instead of running
//! the AI inpainter. Same input/output contract as `lama.rs` (reads
//! Segment + Bubble masks, writes `Image { role: Inpainted }`), so it's a
//! drop-in alternative engine id in the pipeline config — no changes needed
//! anywhere that already knows how to composite an `Inpainted` image.
//!
//! Color detection: walks a border ring just outside the (expanded) mask's
//! bounding box and averages the source pixels there that aren't
//! themselves masked. Works well for the common case (a speech bubble over
//! a mostly-uniform background); busy/textured backgrounds will average out
//! to something less exact, which is an acceptable trade-off for a "quick
//! fill" tool — `lama-manga` remains available for anything that needs the
//! AI inpainter's texture synthesis.

use anyhow::{Result, anyhow};
use image::{DynamicImage, Rgba, RgbaImage};
use koharu_core::{ImageRole, MaskRole, Op};
use koharu_ml::inpainting::expand_mask_for_inpainting;

use crate::pipeline::artifacts::Artifact;
use crate::pipeline::engine::{Engine, EngineCtx, EngineInfo};
use crate::pipeline::engines::support::{
    find_image_node, find_mask_node, image_dimensions, load_source_image, text_node_to_region,
    text_nodes, upsert_image_blob,
};

/// How far outside the mask's bounding box to sample for the "surrounding
/// color", in pixels. Wide enough to clear anti-aliased mask edges, narrow
/// enough to stay local to the bubble rather than picking up unrelated
/// panel content.
const BORDER_RING_PX: i64 = 14;
/// Threshold above which a mask pixel counts as "inside" the region to fill.
const MASK_THRESHOLD: u8 = 32;

/// Shared setup for both fill engines: loads the source/base image and
/// computes the same expanded mask `lama.rs` uses, so both fill modes cover
/// exactly the region the AI inpainter would have.
fn load_base_and_mask(ctx: &EngineCtx<'_>) -> Result<(DynamicImage, image::GrayImage)> {
    let (_, mask_ref) = find_mask_node(ctx.scene, ctx.page, MaskRole::Segment)
        .ok_or_else(|| anyhow!("no Segment mask on page"))?;
    let (_, bubble_ref) = find_mask_node(ctx.scene, ctx.page, MaskRole::Bubble)
        .ok_or_else(|| anyhow!("no Bubble mask on page"))?;
    let mask = ctx.blobs.load_image(&mask_ref)?;
    let bubble_mask = ctx.blobs.load_image(&bubble_ref)?;

    let base = match find_image_node(ctx.scene, ctx.page, ImageRole::Inpainted) {
        Some((_, blob)) => ctx.blobs.load_image(&blob)?,
        None => load_source_image(ctx.scene, ctx.page, ctx.blobs)?,
    };

    let text_blocks = text_nodes(ctx.scene, ctx.page)
        .into_iter()
        .map(|(_, transform, text)| text_node_to_region(transform, text))
        .collect::<Vec<_>>();
    let expanded = expand_mask_for_inpainting(&mask, &bubble_mask, &text_blocks);

    Ok((base, expanded))
}

/// Writes `result` as the page's `Image { Inpainted }`, same as `lama.rs`.
fn write_inpainted(ctx: &EngineCtx<'_>, result: &DynamicImage) -> Result<Op> {
    let (w, h) = image_dimensions(result);
    let blob = ctx.blobs.put_webp(result)?;
    Ok(upsert_image_blob(
        ctx.scene,
        ctx.page,
        ImageRole::Inpainted,
        blob,
        w,
        h,
    ))
}

pub struct DetectFillEngine;

#[async_trait::async_trait]
impl Engine for DetectFillEngine {
    async fn run(&self, ctx: EngineCtx<'_>) -> Result<Vec<Op>> {
        let (base, expanded) = load_base_and_mask(&ctx)?;
        let color = detect_surrounding_color(&base, &expanded);
        let result = fill_mask_with_color(&base, &expanded, color);
        Ok(vec![write_inpainted(&ctx, &result)?])
    }
}

/// Averages the source-image color of pixels in a ring just outside the
/// mask's bounding box (excluding any pixel that is itself masked).
/// Falls back to opaque white if the mask covers the whole image (no ring
/// to sample from).
fn detect_surrounding_color(base: &DynamicImage, mask: &image::GrayImage) -> Rgba<u8> {
    let (w, h) = mask.dimensions();
    let base_rgba = base.to_rgba8();

    let Some((min_x, min_y, max_x, max_y)) = mask_bounds(mask, MASK_THRESHOLD) else {
        return Rgba([255, 255, 255, 255]);
    };

    let rx0 = min_x.saturating_sub(BORDER_RING_PX).max(0) as u32;
    let ry0 = min_y.saturating_sub(BORDER_RING_PX).max(0) as u32;
    let rx1 = ((max_x + BORDER_RING_PX).max(0) as u32).min(w.saturating_sub(1));
    let ry1 = ((max_y + BORDER_RING_PX).max(0) as u32).min(h.saturating_sub(1));

    let (mut r_sum, mut g_sum, mut b_sum, mut count) = (0u64, 0u64, 0u64, 0u64);
    for y in ry0..=ry1 {
        for x in rx0..=rx1 {
            if mask.get_pixel(x, y).0[0] >= MASK_THRESHOLD {
                continue; // still inside the region we're about to fill
            }
            let p = base_rgba.get_pixel(x, y).0;
            r_sum += p[0] as u64;
            g_sum += p[1] as u64;
            b_sum += p[2] as u64;
            count += 1;
        }
    }

    if count == 0 {
        return Rgba([255, 255, 255, 255]);
    }
    Rgba([
        (r_sum / count) as u8,
        (g_sum / count) as u8,
        (b_sum / count) as u8,
        255,
    ])
}

/// Bounding box (min_x, min_y, max_x, max_y) of every pixel `>= threshold`.
fn mask_bounds(mask: &image::GrayImage, threshold: u8) -> Option<(i64, i64, i64, i64)> {
    let (w, h) = mask.dimensions();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x, y).0[0] >= threshold {
                min_x = min_x.min(x as i64);
                min_y = min_y.min(y as i64);
                max_x = max_x.max(x as i64);
                max_y = max_y.max(y as i64);
            }
        }
    }
    (min_x <= max_x).then_some((min_x, min_y, max_x, max_y))
}

/// Returns a copy of `base` with every masked pixel replaced by `color`.
pub(crate) fn fill_mask_with_color(
    base: &DynamicImage,
    mask: &image::GrayImage,
    color: Rgba<u8>,
) -> DynamicImage {
    let mut out: RgbaImage = base.to_rgba8();
    let (w, h) = out.dimensions();
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x, y).0[0] >= MASK_THRESHOLD {
                out.put_pixel(x, y, color);
            }
        }
    }
    DynamicImage::ImageRgba8(out)
}

inventory::submit! {
    EngineInfo {
        id: "mask-fill-detect",
        name: "Preencher (Detectar cor)",
        needs: &[Artifact::SegmentMask, Artifact::BubbleMask],
        produces: &[Artifact::Inpainted],
        load: |_runtime, _cpu| Box::pin(async move {
            Ok(Box::new(DetectFillEngine) as Box<dyn Engine>)
        }),
    }
}

/// "Preencher → Sólido" — same as `DetectFillEngine`, but uses an explicit
/// color (`ctx.options.fill_color`) instead of auto-detecting one. The
/// color picker lives in the UI; the request layer just needs to forward it
/// through `PipelineRunOptions.fill_color`.
pub struct SolidFillEngine;

#[async_trait::async_trait]
impl Engine for SolidFillEngine {
    async fn run(&self, ctx: EngineCtx<'_>) -> Result<Vec<Op>> {
        let color = ctx
            .options
            .fill_color
            .ok_or_else(|| anyhow!("mask-fill-solid requires options.fill_color"))?;

        let (base, expanded) = load_base_and_mask(&ctx)?;
        let result = fill_mask_with_color(&base, &expanded, Rgba(color));
        Ok(vec![write_inpainted(&ctx, &result)?])
    }
}

inventory::submit! {
    EngineInfo {
        id: "mask-fill-solid",
        name: "Preencher (Cor sólida)",
        needs: &[Artifact::SegmentMask, Artifact::BubbleMask],
        produces: &[Artifact::Inpainted],
        load: |_runtime, _cpu| Box::pin(async move {
            Ok(Box::new(SolidFillEngine) as Box<dyn Engine>)
        }),
    }
}
