'use client'

import { CheckIcon, Loader2Icon, XIcon } from 'lucide-react'
import type React from 'react'
import { useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { Button } from '@/components/ui/button'
import type { Page } from '@/lib/api/schemas'
import { cropPageImage, type CropRect } from '@/lib/io/imageCrop'
import { useEditorUiStore } from '@/lib/stores/editorUiStore'

const HANDLE_SIZE = 12
const MIN_SIZE = 16

type ResizeEdge = 'n' | 's' | 'e' | 'w' | 'ne' | 'nw' | 'se' | 'sw'
type DragKind = ResizeEdge | 'move'

const clamp = (v: number, min: number, max: number) => Math.min(Math.max(v, min), max)

/**
 * Crop tool overlay — behaves like Canva/Word's crop, not a "draw a
 * rectangle" tool: the frame starts covering the whole page image, and the
 * user drags its edges/corners inward (or back out) to adjust it, or drags
 * the frame's body to reposition it without resizing. Confirm commits via
 * `cropPageImage`; Cancel/Esc discards and leaves the page untouched.
 *
 * `containerRef` must point at the element sized exactly to the page's
 * on-screen dimensions (`page.width * scale` × `page.height * scale`) — the
 * same element the base image itself is painted into — since all frame
 * math is done in that element's local coordinate space.
 */
export function CropOverlay({
                                page,
                                scale,
                                containerRef,
                                sourceBytes,
                                onCancel,
                            }: {
    page: Page
    scale: number
    containerRef: React.RefObject<HTMLElement | null>
    sourceBytes: Uint8Array | undefined
    onCancel: () => void
}) {
    const { t } = useTranslation()
    const setMode = useEditorUiStore((s) => s.setMode)
    const [frame, setFrame] = useState<CropRect>({
        x: 0,
        y: 0,
        width: page.width,
        height: page.height,
    })
    const [applying, setApplying] = useState(false)
    const [error, setError] = useState<string | null>(null)

    const dragRef = useRef<{
        kind: DragKind
        start: CropRect
        pointerId: number
        x0: number
        y0: number
    } | null>(null)

    const toDocPoint = (clientX: number, clientY: number) => {
        const rect = containerRef.current?.getBoundingClientRect()
        if (!rect) return { x: 0, y: 0 }
        return { x: (clientX - rect.left) / scale, y: (clientY - rect.top) / scale }
    }

    const beginDrag = (kind: DragKind) => (event: React.PointerEvent) => {
        event.stopPropagation()
        event.preventDefault()
        const p = toDocPoint(event.clientX, event.clientY)
        dragRef.current = { kind, start: frame, pointerId: event.pointerId, x0: p.x, y0: p.y }
        ;(event.target as Element).setPointerCapture(event.pointerId)
    }

    const onDrag = (event: React.PointerEvent) => {
        const drag = dragRef.current
        if (!drag || event.pointerId !== drag.pointerId) return
        event.preventDefault()
        const p = toDocPoint(event.clientX, event.clientY)
        const dx = p.x - drag.x0
        const dy = p.y - drag.y0
        const { start } = drag

        if (drag.kind === 'move') {
            const x = clamp(start.x + dx, 0, page.width - start.width)
            const y = clamp(start.y + dy, 0, page.height - start.height)
            setFrame({ x, y, width: start.width, height: start.height })
            return
        }

        const edge = drag.kind
        let { x, y, width, height } = start

        if (edge.includes('n')) {
            const newY = clamp(start.y + dy, 0, start.y + start.height - MIN_SIZE)
            height = start.height - (newY - start.y)
            y = newY
        }
        if (edge.includes('s')) {
            height = clamp(start.height + dy, MIN_SIZE, page.height - start.y)
        }
        if (edge.includes('w')) {
            const newX = clamp(start.x + dx, 0, start.x + start.width - MIN_SIZE)
            width = start.width - (newX - start.x)
            x = newX
        }
        if (edge.includes('e')) {
            width = clamp(start.width + dx, MIN_SIZE, page.width - start.x)
        }

        setFrame({ x, y, width, height })
    }

    const endDrag = () => {
        dragRef.current = null
    }

    const hasChange =
        frame.x !== 0 || frame.y !== 0 || frame.width !== page.width || frame.height !== page.height

    const handleConfirm = async () => {
        if (!sourceBytes || !hasChange) return
        setApplying(true)
        setError(null)
        try {
            await cropPageImage(page, frame, sourceBytes)
            setMode('select')
        } catch (err) {
            setError(err instanceof Error ? err.message : String(err))
            setApplying(false)
        }
    }

    const left = frame.x * scale
    const top = frame.y * scale
    const width = frame.width * scale
    const height = frame.height * scale
    const half = HANDLE_SIZE / 2

    const handles: { edge: ResizeEdge; cursor: string; style: React.CSSProperties }[] = [
        { edge: 'nw', cursor: 'nwse-resize', style: { top: -half, left: -half } },
        { edge: 'ne', cursor: 'nesw-resize', style: { top: -half, right: -half } },
        { edge: 'sw', cursor: 'nesw-resize', style: { bottom: -half, left: -half } },
        { edge: 'se', cursor: 'nwse-resize', style: { bottom: -half, right: -half } },
        { edge: 'n', cursor: 'ns-resize', style: { top: -half, left: '50%', marginLeft: -half } },
        { edge: 's', cursor: 'ns-resize', style: { bottom: -half, left: '50%', marginLeft: -half } },
        { edge: 'w', cursor: 'ew-resize', style: { left: -half, top: '50%', marginTop: -half } },
        { edge: 'e', cursor: 'ew-resize', style: { right: -half, top: '50%', marginTop: -half } },
    ]

    return (
        <div
            className='absolute inset-0 z-40'
            onPointerMove={onDrag}
            onPointerUp={endDrag}
            onPointerCancel={endDrag}
        >
            {/* Dim everything outside the frame — four plain rects, no clip-path. */}
            <div
                className='pointer-events-none absolute inset-x-0 top-0 bg-black/50'
                style={{ height: top }}
            />
            <div
                className='pointer-events-none absolute inset-x-0 bottom-0 bg-black/50'
                style={{ top: top + height }}
            />
            <div
                className='pointer-events-none absolute bg-black/50'
                style={{ left: 0, top, width: left, height }}
            />
            <div
                className='pointer-events-none absolute bg-black/50'
                style={{ left: left + width, top, right: 0, height }}
            />

            {/* The frame itself — also draggable (moves without resizing). */}
            <div
                data-testid='crop-frame'
                className='absolute cursor-move border-2 border-primary'
                style={{ left, top, width, height }}
                onPointerDown={beginDrag('move')}
            >
                {/* Rule-of-thirds guides, purely visual. */}
                <div className='pointer-events-none absolute inset-0 grid grid-cols-3 grid-rows-3'>
                    {Array.from({ length: 9 }).map((_, i) => (
                        <div key={i} className='border-white/30' style={{ borderWidth: '0 1px 1px 0' }} />
                    ))}
                </div>

                {handles.map((h) => (
                    <div
                        key={h.edge}
                        data-testid={`crop-handle-${h.edge}`}
                        onPointerDown={beginDrag(h.edge)}
                        className='absolute rounded-sm border-2 border-primary bg-background shadow'
                        style={{ ...h.style, width: HANDLE_SIZE, height: HANDLE_SIZE, cursor: h.cursor }}
                    />
                ))}
            </div>

            <div
                className='absolute z-50 flex items-center gap-1 rounded-md border border-border bg-popover p-1 shadow-md'
                style={{ left, top: top + height + 8 }}
            >
                <Button
                    size='sm'
                    className='h-7 text-xs'
                    disabled={applying || !sourceBytes || !hasChange}
                    data-testid='crop-apply'
                    onClick={() => void handleConfirm()}
                >
                    {applying ? (
                        <Loader2Icon className='size-3.5 animate-spin' />
                    ) : (
                        <CheckIcon className='size-3.5' />
                    )}
                    {t('crop.apply')}
                </Button>
                <Button
                    size='sm'
                    variant='outline'
                    className='h-7 text-xs'
                    disabled={applying}
                    data-testid='crop-cancel'
                    onClick={onCancel}
                >
                    <XIcon className='size-3.5' />
                    {t('crop.cancel')}
                </Button>
            </div>

            {error && (
                <div
                    className='absolute z-50 max-w-xs rounded-md border border-destructive bg-destructive/10 p-2 text-xs text-destructive'
                    style={{ left, top: top + height + 44 }}
                >
                    {error}
                </div>
            )}
        </div>
    )
}
