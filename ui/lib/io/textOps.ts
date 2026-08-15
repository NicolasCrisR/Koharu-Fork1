import { isTextNode } from '@/hooks/useCurrentPage'
import type { Op, Scene, TextStyle } from '@/lib/api/schemas'
import { ops } from '@/lib/ops'

export type TextNodeRef = {
    pageId: string
    nodeId: string
    text: string
}

/**
 * Every text block across every page in the scene, with its current
 * translation text. Mirrors the cross-page scan already used by the font
 * replace panel (`scene.pages` ships every page to the client, so no
 * backend endpoint is needed).
 */
export function collectAllTextNodes(scene: Scene | null): TextNodeRef[] {
    if (!scene) return []
    const out: TextNodeRef[] = []
    for (const page of Object.values(scene.pages)) {
        for (const [nodeId, node] of Object.entries(page.nodes)) {
            if (!isTextNode(node)) continue
            out.push({ pageId: page.id, nodeId, text: node.kind.text.translation ?? '' })
        }
    }
    return out
}

/**
 * Builds a single `Op::Batch` applying `transform` to every ref whose
 * result actually differs from the current text (so untouched blocks don't
 * generate no-op patches / undo noise).
 */
export function buildTranslationBatchOp(
    refs: TextNodeRef[],
    transform: (text: string) => string,
    label: string,
): { op: Op | null; affectedPageIds: string[]; affectedNodeCount: number } {
    const nodeOps: Op[] = []
    const pages = new Set<string>()

    for (const ref of refs) {
        const next = transform(ref.text)
        if (next === ref.text) continue
        nodeOps.push(ops.updateNode(ref.pageId, ref.nodeId, { data: { text: { translation: next } } } as never))
        pages.add(ref.pageId)
    }

    if (nodeOps.length === 0) return { op: null, affectedPageIds: [], affectedNodeCount: 0 }

    const op = nodeOps.length === 1 ? nodeOps[0] : ops.batch(label, nodeOps)
    return { op, affectedPageIds: Array.from(pages), affectedNodeCount: nodeOps.length }
}

/**
 * Same "one Op::Batch across every page" approach as
 * `buildTranslationBatchOp`, but for style patches (font, spacing, etc.)
 * instead of the translation text. Every existing style field is carried
 * through unless `updates` overrides it, so — e.g. — a global line-spacing
 * change never clobbers a block's font or color.
 */
export function buildStyleBatchOp(
    scene: Scene | null,
    updates: Partial<TextStyle>,
    label: string,
): { op: Op | null; affectedPageIds: string[]; affectedNodeCount: number } {
    if (!scene) return { op: null, affectedPageIds: [], affectedNodeCount: 0 }

    const nodeOps: Op[] = []
    const pages = new Set<string>()

    for (const page of Object.values(scene.pages)) {
        for (const [nodeId, node] of Object.entries(page.nodes)) {
            if (!isTextNode(node)) continue
            const current = node.kind.text.style
            const nextStyle: TextStyle = {
                fontFamilies: updates.fontFamilies ?? current?.fontFamilies ?? [],
                fontSize: updates.fontSize ?? current?.fontSize ?? null,
                color: updates.color ?? current?.color ?? [0, 0, 0, 255],
                effect: updates.effect ?? current?.effect ?? null,
                stroke: updates.stroke ?? current?.stroke ?? null,
                textAlign: updates.textAlign ?? current?.textAlign ?? null,
                lineSpacing: updates.lineSpacing ?? current?.lineSpacing ?? null,
                letterSpacing: updates.letterSpacing ?? current?.letterSpacing ?? null,
            }
            nodeOps.push(
                ops.updateNode(page.id, nodeId, { data: { text: { style: nextStyle } } } as never),
            )
            pages.add(page.id)
        }
    }

    if (nodeOps.length === 0) return { op: null, affectedPageIds: [], affectedNodeCount: 0 }

    const op = nodeOps.length === 1 ? nodeOps[0] : ops.batch(label, nodeOps)
    return { op, affectedPageIds: Array.from(pages), affectedNodeCount: nodeOps.length }
}