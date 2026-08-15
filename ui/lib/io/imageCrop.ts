import { findImageNodeId, findMaskNodeId, textNodesOf } from '@/hooks/useCurrentPage'
import { addImageLayer, getSceneJson } from '@/lib/api/default/default'
import type { ImageRole, MaskRole, Op, Page } from '@/lib/api/schemas'
import { convertToBlob } from '@/lib/io/blobConvert'
import { applyOp } from '@/lib/io/scene'
import { ops } from '@/lib/ops'

export type CropRect = { x: number; y: number; width: number; height: number }

const DEPENDENT_IMAGE_ROLES: ImageRole[] = ['inpainted', 'rendered']
const DEPENDENT_MASK_ROLES: MaskRole[] = ['segment', 'bubble', 'brushInpaint']

/** Draws `rect` of `sourceBytes` onto an off-screen canvas and returns a PNG Blob. */
async function cropImageBytes(sourceBytes: Uint8Array, rect: CropRect): Promise<Blob> {
    const srcBlob = await convertToBlob(sourceBytes)
    const url = URL.createObjectURL(srcBlob)
    try {
        const img = await new Promise<HTMLImageElement>((resolve, reject) => {
            const el = new window.Image()
            el.onload = () => resolve(el)
            el.onerror = () => reject(new Error('Failed to decode source image for crop'))
            el.src = url
        })

        const width = Math.max(1, Math.round(rect.width))
        const height = Math.max(1, Math.round(rect.height))
        const canvas = document.createElement('canvas')
        canvas.width = width
        canvas.height = height
        const ctx = canvas.getContext('2d')
        if (!ctx) throw new Error('2D canvas context unavailable')
        ctx.drawImage(img, rect.x, rect.y, rect.width, rect.height, 0, 0, width, height)

        return await new Promise<Blob>((resolve, reject) => {
            canvas.toBlob(
                (blob) => (blob ? resolve(blob) : reject(new Error('Canvas toBlob failed'))),
                'image/png',
            )
        })
    } finally {
        URL.revokeObjectURL(url)
    }
}

function nodeIndex(page: Page, nodeId: string): number {
    const idx = Object.keys(page.nodes).indexOf(nodeId)
    return idx < 0 ? 0 : idx
}

/**
 * Crops the page's source image to `rect` (in page/document pixel coords)
 * and commits every dependent change as a single `Op::Batch` (one undo
 * step):
 *
 * 1. Uploads the cropped PNG via `POST /pages/{id}/image-layers` — that
 *    endpoint's real job is "add an extra layer", but it already does
 *    exactly the "receive bytes → hash → blob" work we need, so we reuse it
 *    purely as an upload vehicle and delete the layer node it creates right
 *    after reading its blob hash + dimensions back.
 * 2. Swaps the page's `source` image node to the new blob/dimensions.
 * 3. Resizes the page to the crop's dimensions.
 * 4. Translates every text block by `(-rect.x, -rect.y)`. A block that ends
 *    up entirely outside the new bounds is dropped; anything still
 *    (partially) inside is kept and repositioned — cropping a margin
 *    shouldn't cost you translation work that's still valid.
 * 5. Removes the segmentation/bubble/brush masks and the inpainted/rendered
 *    images. They were computed against pixels that no longer exist at
 *    those coordinates, so keeping them would silently show stale/wrong
 *    content instead of failing loudly. The page needs a pipeline re-run
 *    after cropping.
 */
export async function cropPageImage(page: Page, rect: CropRect, sourceBytes: Uint8Array) {
    const sourceNodeId = findImageNodeId(page, 'source')
    if (!sourceNodeId) throw new Error('Page has no source image to crop')

    const croppedBlob = await cropImageBytes(sourceBytes, rect)

    const form = new FormData()
    form.append('file', croppedBlob, 'crop.png')
    const { node: tempNodeId } = await addImageLayer(page.id, { body: form })

    // `addImageLayer` only returns the new node's id — re-read the scene to
    // learn the blob hash + natural dimensions the backend derived from it.
    const snapshot = await getSceneJson()
    const freshPage = snapshot.scene.pages[page.id]
    const tempNode = freshPage?.nodes[tempNodeId]
    if (!freshPage || !tempNode || !('image' in tempNode.kind)) {
        throw new Error('Upload succeeded but the new layer node could not be read back')
    }
    const { blob, naturalWidth, naturalHeight } = tempNode.kind.image
    if (!blob || !naturalWidth || !naturalHeight) {
        throw new Error('Uploaded layer is missing blob/dimensions')
    }

    const batchOps: Op[] = [
        ops.updateImage(freshPage.id, sourceNodeId, { blob, naturalWidth, naturalHeight }),
        ops.updatePage(freshPage.id, { width: naturalWidth, height: naturalHeight }),
        ops.removeNode(freshPage.id, tempNodeId, tempNode, nodeIndex(freshPage, tempNodeId)),
    ]

    for (const role of DEPENDENT_IMAGE_ROLES) {
        const id = findImageNodeId(freshPage, role)
        if (!id) continue
        batchOps.push(ops.removeNode(freshPage.id, id, freshPage.nodes[id], nodeIndex(freshPage, id)))
    }
    for (const role of DEPENDENT_MASK_ROLES) {
        const id = findMaskNodeId(freshPage, role)
        if (!id) continue
        batchOps.push(ops.removeNode(freshPage.id, id, freshPage.nodes[id], nodeIndex(freshPage, id)))
    }

    for (const t of textNodesOf(freshPage)) {
        const nx = t.transform.x - rect.x
        const ny = t.transform.y - rect.y
        const stillVisible =
            nx + t.transform.width > 0 &&
            ny + t.transform.height > 0 &&
            nx < naturalWidth &&
            ny < naturalHeight

        if (!stillVisible) {
            batchOps.push(
                ops.removeNode(freshPage.id, t.id, freshPage.nodes[t.id], nodeIndex(freshPage, t.id)),
            )
            continue
        }
        batchOps.push(ops.updateNode(freshPage.id, t.id, { transform: { ...t.transform, x: nx, y: ny } }))
    }

    await applyOp(ops.batch('Crop page image', batchOps))
}