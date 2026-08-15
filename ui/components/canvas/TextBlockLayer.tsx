'use client'

import { useDrag } from '@use-gesture/react'
import { useMemo, useRef } from 'react'
import { createPortal } from 'react-dom'
import { useHotkeys } from 'react-hotkeys-hook'

import { useBlobImage } from '@/hooks/useBlobData'
import { useCurrentPage, useTextNodes, type TextNodeEntry } from '@/hooks/useCurrentPage'
import type { NodeDataPatch, Transform } from '@/lib/api/schemas'
import { applyOp, queueAutoRender } from '@/lib/io/scene'
import { ops } from '@/lib/ops'
import { useEditorUiStore } from '@/lib/stores/editorUiStore'
import { useSelectionStore } from '@/lib/stores/selectionStore'

type TextBlockLayerProps = {
    showSprites?: boolean
    scale: number
    style?: React.CSSProperties
}

/**
 * Overlay for the active page's Text nodes. Each rectangle is draggable /
 * resizable; commits dispatch `Op::UpdateNode { transform }` through
 * `applyCommand`. Selection is driven by `selectionStore.nodeIds`.
 */
export function TextBlockLayer({ showSprites, scale, style }: TextBlockLayerProps) {
    const nodes = useTextNodes()
    const page = useCurrentPage()
    const selectedIds = useSelectionStore((s) => s.nodeIds)
    const select = useSelectionStore((s) => s.select)
    const mode = useEditorUiStore((s) => s.mode)
    const interactive = mode === 'select' || mode === 'block'

    const hasSelection = useMemo(() => {
        for (const id of selectedIds) if (id) return true
        return false
    }, [selectedIds])

    const removeNode = async (id: string) => {
        if (!page) return
        const node = page.nodes[id]
        if (!node) return
        const idx = Object.keys(page.nodes).indexOf(id)
        await applyOp(ops.removeNode(page.id, id, node, idx < 0 ? 0 : idx))
        if ('text' in node.kind) queueAutoRender(page.id)
    }

    const removeSelected = async () => {
        if (!page) return
        // Snapshot selection now: each op invalidates the page state by removing a
        // node, so we can't iterate against a stale closure mid-loop.
        const ids = Array.from(selectedIds).filter((id): id is string => !!id)
        for (const id of ids) {
            await removeNode(id)
        }
    }

    const updateTransform = async (id: string, t: Transform) => {
        if (!page) return
        const data: NodeDataPatch = {
            text: {
                lockLayoutBox: true,
            },
        }
        await applyOp(ops.updateNode(page.id, id, { transform: t, data }))
        queueAutoRender(page.id)
    }

    useHotkeys(
        'delete',
        () => {
            if (hasSelection && interactive) void removeSelected()
        },
        { enabled: hasSelection && interactive },
        [selectedIds, interactive],
    )

    return (
        <div
            data-text-block-layer
            style={{
                ...style,
                position: 'absolute',
                inset: 0,
                width: '100%',
                height: '100%',
                pointerEvents: 'none',
            }}
        >
            {showSprites &&
                nodes.map((n, i) => <BlockSprite key={`sprite-${n.id ?? i}`} node={n} scale={scale} />)}
            {nodes.map((n, i) => (
                <TextBlockItem
                    key={n.id}
                    node={n}
                    index={i}
                    scale={scale}
                    selected={selectedIds.has(n.id)}
                    interactive={interactive}
                    onSelect={(id, additive) => select(id, additive)}
                    onCommit={(t) => void updateTransform(n.id, t)}
                />
            ))}
        </div>
    )
}

type TextBlockItemProps = {
    node: TextNodeEntry
    index: number
    scale: number
    selected: boolean
    interactive: boolean
    onSelect: (id: string, additive: boolean) => void
    onCommit: (transform: Transform) => void
}

const isAdditiveEvent = (event: unknown): boolean => {
    if (!event || typeof event !== 'object') return false
    const e = event as { shiftKey?: boolean; metaKey?: boolean; ctrlKey?: boolean }
    return !!(e.shiftKey || e.metaKey || e.ctrlKey)
}

const isShiftHeld = (event: unknown): boolean => {
    if (!event || typeof event !== 'object') return false
    return !!(event as { shiftKey?: boolean }).shiftKey
}

const RESIZE_HANDLE_SIZE = 8

type ResizeEdge = { top: boolean; bottom: boolean; left: boolean; right: boolean }

function TextBlockItem({
                           node,
                           index,
                           scale,
                           selected,
                           interactive,
                           onSelect,
                           onCommit,
                       }: TextBlockItemProps) {
    const boxRef = useRef<HTMLDivElement>(null)
    const dragStart = useRef({ x: 0, y: 0, w: 0, h: 0 })
    const edgeRef = useRef<ResizeEdge | null>(null)
    const isResizeRef = useRef(false)
    const isRotatingRef = useRef(false)
    const rotateStart = useRef({ pointerAngle: 0, rotation: 0, cx: 0, cy: 0 })
    const lastRotationRef = useRef(0)
    const angleBadgeRef = useRef<HTMLDivElement>(null)
    const guideHRef = useRef<HTMLDivElement>(null)
    const guideVRef = useRef<HTMLDivElement>(null)

    // Imperatively position/label the rotation-angle badge next to the
    // pointer, mirroring `setBox`'s "skip React during drag" approach so the
    // badge tracks the cursor at 60fps without triggering re-renders.
    const updateAngleBadge = (clientX: number, clientY: number, angle: number) => {
        const el = angleBadgeRef.current
        if (!el) return
        el.style.left = `${clientX + 18}px`
        el.style.top = `${clientY + 18}px`
        el.textContent = `${Math.round(angle)}°`
    }

    // Shift-lock axis guides: a full-viewport horizontal + vertical line
    // through the box's own center, shown only while Shift is held during a
    // move. Same imperative, no-React-state approach as `setBox` — these
    // need to move every pointer frame, not just on commit.
    const setAxisGuides = (visible: boolean) => {
        const h = guideHRef.current
        const v = guideVRef.current
        if (!h || !v) return
        if (!visible) {
            h.style.display = 'none'
            v.style.display = 'none'
            return
        }
        const rect = boxRef.current?.getBoundingClientRect()
        if (!rect) return
        const cx = rect.left + rect.width / 2
        const cy = rect.top + rect.height / 2
        h.style.display = 'block'
        h.style.top = `${cy}px`
        v.style.display = 'block'
        v.style.left = `${cx}px`
    }

    const setBox = (x: number, y: number, w: number, h: number, rotation: number) => {
        const el = boxRef.current
        if (!el) return
        el.style.transform = `translate(${x}px, ${y}px) rotate(${rotation}deg)`
        el.style.width = `${w}px`
        el.style.height = `${h}px`
    }

    const t = node.transform
    lastRotationRef.current = t.rotationDeg ?? 0

    const bind = useDrag(
        ({ first, last, movement: [mx, my], event, tap }) => {
            if (!interactive || isRotatingRef.current) return
            event?.stopPropagation()
            const additive = isAdditiveEvent(event)
            if (tap) {
                onSelect(node.id, additive)
                return
            }
            if (first) {
                dragStart.current = {
                    x: t.x * scale,
                    y: t.y * scale,
                    w: t.width * scale,
                    h: t.height * scale,
                }
                // Keep multi-selection intact when dragging a node that's already selected;
                // otherwise this click is a single-select (unless the modifier is held).
                if (additive || !selected) onSelect(node.id, additive)
            }
            const { x: sx, y: sy, w: sw, h: sh } = dragStart.current
            const edge = edgeRef.current
            if (isResizeRef.current && edge) {
                let dx = 0
                let dy = 0
                let w = sw
                let h = sh
                if (edge.right) w += mx
                if (edge.left) {
                    w -= mx
                    dx = mx
                }
                if (edge.bottom) h += my
                if (edge.top) {
                    h -= my
                    dy = my
                }
                w = Math.max(4 * scale, w)
                h = Math.max(4 * scale, h)
                if (edge.left && w === 4 * scale) dx = sw - 4 * scale
                if (edge.top && h === 4 * scale) dy = sh - 4 * scale
                setBox(sx + dx, sy + dy, w, h, t.rotationDeg ?? 0)
                if (last) {
                    isResizeRef.current = false
                    edgeRef.current = null
                    onCommit({
                        x: Math.round((sx + dx) / scale),
                        y: Math.round((sy + dy) / scale),
                        width: Math.max(4, Math.round(w / scale)),
                        height: Math.max(4, Math.round(h / scale)),
                        rotationDeg: t.rotationDeg ?? 0,
                    })
                }
            } else {
                // Shift locks movement to a straight horizontal or vertical line,
                // following whichever axis has moved the most since drag start
                // (re-evaluated every frame, so it can "switch" axis early in the
                // drag the same way Canva/Figma do).
                let dx = mx
                let dy = my
                const shiftLocked = isShiftHeld(event)
                if (shiftLocked) {
                    if (Math.abs(mx) >= Math.abs(my)) {
                        dy = 0
                    } else {
                        dx = 0
                    }
                }
                setBox(sx + dx, sy + dy, sw, sh, t.rotationDeg ?? 0)
                setAxisGuides(shiftLocked && !last)
                if (last) {
                    onCommit({
                        x: Math.round((sx + dx) / scale),
                        y: Math.round((sy + dy) / scale),
                        width: t.width,
                        height: t.height,
                        rotationDeg: t.rotationDeg ?? 0,
                    })
                }
            }
        },
        {
            pointer: { buttons: 1, touch: true },
            filterTaps: true,
            preventDefault: true,
            eventOptions: { passive: false },
        },
    )

    const handleEdgePointerDown = (edge: ResizeEdge) => {
        if (!interactive || !selected) return
        isResizeRef.current = true
        edgeRef.current = edge
    }

    // Rotation handle drag: angle is measured from the box's screen-space
    // center, so it stays correct at any zoom level. We track the delta
    // between the pointer's angle and the box's rotation at grab time so the
    // handle doesn't "snap" to the cursor the moment the drag starts.
    const handleRotatePointerDown = (event: React.PointerEvent) => {
        if (!interactive || !selected) return
        event.stopPropagation()
        event.preventDefault()
        const el = boxRef.current
        if (!el) return
        const rect = el.getBoundingClientRect()
        const cx = rect.left + rect.width / 2
        const cy = rect.top + rect.height / 2
        const pointerAngle = (Math.atan2(event.clientY - cy, event.clientX - cx) * 180) / Math.PI
        rotateStart.current = { pointerAngle, rotation: t.rotationDeg ?? 0, cx, cy }
        isRotatingRef.current = true
        ;(event.target as Element).setPointerCapture(event.pointerId)
        if (angleBadgeRef.current) angleBadgeRef.current.style.display = 'block'
        updateAngleBadge(event.clientX, event.clientY, t.rotationDeg ?? 0)
    }

    const handleRotatePointerMove = (event: React.PointerEvent) => {
        if (!isRotatingRef.current) return
        event.stopPropagation()
        const { pointerAngle, rotation, cx, cy } = rotateStart.current
        const currentAngle = (Math.atan2(event.clientY - cy, event.clientX - cx) * 180) / Math.PI
        let next = rotation + (currentAngle - pointerAngle)
        // Hold Shift to snap to 15° increments, matching common design-tool conventions.
        if (isShiftHeld(event)) next = Math.round(next / 15) * 15
        next = ((next % 360) + 360) % 360
        lastRotationRef.current = next
        setBox(t.x * scale, t.y * scale, t.width * scale, t.height * scale, next)
        updateAngleBadge(event.clientX, event.clientY, next)
    }

    const handleRotatePointerUp = (event: React.PointerEvent) => {
        if (!isRotatingRef.current) return
        event.stopPropagation()
        isRotatingRef.current = false
        if (angleBadgeRef.current) angleBadgeRef.current.style.display = 'none'
        onCommit({
            x: t.x,
            y: t.y,
            width: t.width,
            height: t.height,
            rotationDeg: lastRotationRef.current,
        })
    }

    const w = t.width * scale
    const h = t.height * scale

    return (
        <>
            <div
                ref={boxRef}
                {...bind()}
                style={{
                    position: 'absolute',
                    top: 0,
                    left: 0,
                    transform: `translate(${t.x * scale}px, ${t.y * scale}px) rotate(${t.rotationDeg ?? 0}deg)`,
                    transformOrigin: 'center',
                    width: w,
                    height: h,
                    pointerEvents: interactive ? 'auto' : 'none',
                    zIndex: selected ? 20 : 10,
                    touchAction: 'none',
                    cursor: interactive ? 'move' : 'default',
                }}
            >
                <div
                    className={`absolute inset-0 rounded-md ${
                        selected
                            ? 'border-[3px] border-primary bg-primary/15'
                            : 'border-2 border-rose-400/60 bg-rose-400/5'
                    }`}
                />
                <div
                    className={`pointer-events-none absolute -top-1.5 -left-1.5 flex h-4 w-4 items-center justify-center rounded-full text-[9px] font-semibold text-white shadow ${
                        selected ? 'bg-primary' : 'bg-rose-400'
                    }`}
                >
                    {index + 1}
                </div>
                {selected && interactive && (
                    <>
                        <ResizeHandles onEdgePointerDown={handleEdgePointerDown} />
                        <RotateHandle
                            onPointerDown={handleRotatePointerDown}
                            onPointerMove={handleRotatePointerMove}
                            onPointerUp={handleRotatePointerUp}
                        />
                    </>
                )}
            </div>
            {/*
             * Rendered outside the rotated box (via portal) on purpose: an
             * ancestor with a CSS `transform` becomes the containing block
             * for `position: fixed`, so nesting this inside `boxRef` would
             * make it spin along with the box and defeat the point of an
             * "upright, always readable" angle readout. Always mounted,
             * toggled with `display` and updated imperatively (no React
             * state) so it can track the pointer every frame without
             * re-rendering the tree during drag — same trick as `setBox`.
             */}
            {typeof document !== 'undefined' &&
                createPortal(
                    <div
                        ref={angleBadgeRef}
                        style={{ display: 'none' }}
                        className='pointer-events-none fixed z-[1000] rounded-md bg-primary px-1.5 py-0.5 text-[11px] font-semibold tabular-nums text-primary-foreground shadow'
                    >
                        0°
                    </div>,
                    document.body,
                )}
            {/* Shift-lock axis guides — same "portal + imperative style" trick as the angle badge above. */}
            {typeof document !== 'undefined' &&
                createPortal(
                    <>
                        <div
                            ref={guideHRef}
                            style={{ display: 'none' }}
                            className='pointer-events-none fixed inset-x-0 z-[999] h-px bg-primary/70'
                        />
                        <div
                            ref={guideVRef}
                            style={{ display: 'none' }}
                            className='pointer-events-none fixed inset-y-0 z-[999] w-px bg-primary/70'
                        />
                    </>,
                    document.body,
                )}
        </>
    )
}

function BlockSprite({ node, scale }: { node: TextNodeEntry; scale: number }) {
    const sprite = (node.data.sprite as string | null | undefined) ?? undefined
    const { data: src } = useBlobImage(sprite)
    if (!src) return null
    const spriteT = node.data.spriteTransform
    const x = (spriteT?.x ?? node.transform.x) * scale
    const y = (spriteT?.y ?? node.transform.y) * scale
    return (
        <img
            alt=''
            src={src}
            draggable={false}
            className='pointer-events-none absolute select-none'
            style={{
                top: 0,
                left: 0,
                transformOrigin: 'top left',
                transform: `translate(${x}px, ${y}px) scale(${scale})`,
            }}
        />
    )
}

function RotateHandle({
                          onPointerDown,
                          onPointerMove,
                          onPointerUp,
                      }: {
    onPointerDown: (event: React.PointerEvent) => void
    onPointerMove: (event: React.PointerEvent) => void
    onPointerUp: (event: React.PointerEvent) => void
}) {
    return (
        <>
            {/* Stem connecting the box's top edge to the rotate handle. */}
            <div
                className='bg-primary/70'
                style={{
                    position: 'absolute',
                    top: -20,
                    left: '50%',
                    width: 2,
                    height: 20,
                    marginLeft: -1,
                    pointerEvents: 'none',
                }}
            />
            <div
                onPointerDown={onPointerDown}
                onPointerMove={onPointerMove}
                onPointerUp={onPointerUp}
                onPointerCancel={onPointerUp}
                className='border-2 border-primary bg-background'
                style={{
                    position: 'absolute',
                    top: -28,
                    left: '50%',
                    width: 12,
                    height: 12,
                    marginLeft: -6,
                    borderRadius: '9999px',
                    cursor: 'grab',
                    zIndex: 30,
                    touchAction: 'none',
                }}
            />
        </>
    )
}

function ResizeHandles({ onEdgePointerDown }: { onEdgePointerDown: (edge: ResizeEdge) => void }) {
    const s = RESIZE_HANDLE_SIZE
    const half = s / 2

    const edges: { edge: ResizeEdge; style: React.CSSProperties; cursor: string }[] = [
        {
            edge: { top: true, left: true, bottom: false, right: false },
            cursor: 'nwse-resize',
            style: { top: -half, left: -half, width: s, height: s },
        },
        {
            edge: { top: true, left: false, bottom: false, right: true },
            cursor: 'nesw-resize',
            style: { top: -half, right: -half, width: s, height: s },
        },
        {
            edge: { top: false, left: true, bottom: true, right: false },
            cursor: 'nesw-resize',
            style: { bottom: -half, left: -half, width: s, height: s },
        },
        {
            edge: { top: false, left: false, bottom: true, right: true },
            cursor: 'nwse-resize',
            style: { bottom: -half, right: -half, width: s, height: s },
        },
        {
            edge: { top: true, left: false, bottom: false, right: false },
            cursor: 'ns-resize',
            style: { top: -half, left: s, right: s, height: s },
        },
        {
            edge: { top: false, left: false, bottom: true, right: false },
            cursor: 'ns-resize',
            style: { bottom: -half, left: s, right: s, height: s },
        },
        {
            edge: { top: false, left: true, bottom: false, right: false },
            cursor: 'ew-resize',
            style: { left: -half, top: s, bottom: s, width: s },
        },
        {
            edge: { top: false, left: false, bottom: false, right: true },
            cursor: 'ew-resize',
            style: { right: -half, top: s, bottom: s, width: s },
        },
    ]

    return (
        <>
            {edges.map((e, i) => (
                <div
                    key={i}
                    onPointerDown={() => onEdgePointerDown(e.edge)}
                    style={{ position: 'absolute', ...e.style, cursor: e.cursor, zIndex: 30 }}
                />
            ))}
        </>
    )
}