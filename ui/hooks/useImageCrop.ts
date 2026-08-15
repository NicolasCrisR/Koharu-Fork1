'use client'

import { useDrag } from '@use-gesture/react'
import { useEffect, useRef, useState } from 'react'

import type { DocumentPointer, PointerToDocumentFn } from '@/hooks/usePointerToDocument'
import type { Page } from '@/lib/api/schemas'
import type { ToolMode } from '@/lib/types'

export type CropDraft = { x: number; y: number; width: number; height: number }
export type CropEdge = { top: boolean; bottom: boolean; left: boolean; right: boolean }

type CropSelectionOptions = {
    mode: ToolMode
    page: Page | null
    pointerToDocument: PointerToDocumentFn
}

const MIN_SIZE = 8

const fullPageFrame = (page: Page): CropDraft => ({
    x: 0,
    y: 0,
    width: page.width,
    height: page.height,
})

/**
 * Word/Canva-style crop: the frame starts covering the *entire* page image,
 * and the user shrinks it inward by dragging a corner/edge handle (or pans
 * it by dragging the frame's interior) — never draws a rectangle from
 * scratch. Nothing is committed on release; the caller (`CropOverlay`)
 * shows a confirm/cancel bar and only calls `cropPageImage` when the user
 * explicitly confirms, since this is destructive.
 */
export function useCropSelection({ mode, page, pointerToDocument }: CropSelectionOptions) {
    const [draft, setDraft] = useState<CropDraft | null>(null)
    const dragStartRef = useRef<CropDraft | null>(null)
    const edgeRef = useRef<CropEdge | null>(null)
    const moveGrabRef = useRef<DocumentPointer>({ x: 0, y: 0 })

    // Entering crop mode (or switching to a different page while in it) always
    // resets to the full image — this mirrors Word/Canva, which never
    // remembers a previous crop rectangle across re-opening the tool.
    useEffect(() => {
        if (mode === 'crop' && page) {
            setDraft(fullPageFrame(page))
        } else {
            setDraft(null)
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [mode, page?.id])

    const clearDraft = () => setDraft(null)

    // Called from a handle's onPointerDown, which (same trick TextBlockLayer
    // uses) fires *before* the bubbled event reaches this frame's own
    // `bind()`-attached listener below — so by the time `first` runs,
    // `edgeRef.current` already reflects which handle (if any) was grabbed.
    const startResize = (edge: CropEdge) => {
        edgeRef.current = edge
    }

    const bind = useDrag(
        ({ first, last, event }) => {
            if (!page || mode !== 'crop') return
            const sourceEvent = event as MouseEvent
            const point = pointerToDocument(sourceEvent)
            if (!point) return
            const px = Math.min(Math.max(point.x, 0), page.width)
            const py = Math.min(Math.max(point.y, 0), page.height)

            if (first) {
                dragStartRef.current = draft ?? fullPageFrame(page)
                moveGrabRef.current = { x: px - dragStartRef.current.x, y: py - dragStartRef.current.y }
                return
            }

            const start = dragStartRef.current
            if (!start) return

            if (edgeRef.current) {
                const edge = edgeRef.current
                let x1 = start.x
                let y1 = start.y
                let x2 = start.x + start.width
                let y2 = start.y + start.height
                if (edge.left) x1 = Math.min(px, x2 - MIN_SIZE)
                if (edge.right) x2 = Math.max(px, x1 + MIN_SIZE)
                if (edge.top) y1 = Math.min(py, y2 - MIN_SIZE)
                if (edge.bottom) y2 = Math.max(py, y1 + MIN_SIZE)
                setDraft({ x: x1, y: y1, width: x2 - x1, height: y2 - y1 })
            } else {
                // Default (no handle grabbed): drag the frame's interior to pan it,
                // bounded so it can't be dragged past the page's edges.
                const { width, height } = start
                const x = Math.min(Math.max(px - moveGrabRef.current.x, 0), page.width - width)
                const y = Math.min(Math.max(py - moveGrabRef.current.y, 0), page.height - height)
                setDraft({ x, y, width, height })
            }

            if (last) {
                edgeRef.current = null
                dragStartRef.current = null
            }
        },
        {
            pointer: { buttons: 1, touch: true },
            preventDefault: true,
            filterTaps: true,
            eventOptions: { passive: false },
        },
    )

    return { cropDraft: draft, bind, startResize, clearDraft }
}