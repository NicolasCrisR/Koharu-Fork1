'use client'

import { useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { Image } from '@/components/Image'
import { TextBlockLayer } from '@/components/canvas/TextBlockLayer'
import { useBlobData } from '@/hooks/useBlobData'
import { findImageBlob } from '@/hooks/useCurrentPage'
import { useScene } from '@/hooks/useScene'
import type { Page } from '@/lib/api/schemas'
import { useEditorUiStore } from '@/lib/stores/editorUiStore'
import { useSelectionStore } from '@/lib/stores/selectionStore'

/**
 * One page's slot in the continuous strip. Each page keeps its own native
 * width/height (scaled uniformly by the shared zoom level), so pages of
 * different sizes stack naturally — no forced common width.
 *
 * `overflow: visible` is intentional: a text block whose sprite bleeds past
 * this page's bottom edge (because the source art wasn't actually cut
 * there) should keep painting into the next page's area rather than being
 * clipped at this page's boundary. `zIndex` descends page-by-page so an
 * earlier page's overflow paints over the page that follows it, matching
 * DOM/paint order to reading order.
 *
 * NOTE: the *rendered* image itself (baked in by the backend) is still
 * clipped to its own page's canvas server-side — `imageops::overlay` clips
 * to destination bounds. So this fixes the preview's clipping, but a block
 * that already got cut off during rendering will still show that cut in
 * the image content. Fully solving that needs a backend change; out of
 * scope for this frontend-only pass.
 *
 * Single click selects the page (shows its text blocks right here, without
 * leaving continuous view — `TextBlockLayer` reads whichever page is
 * `selectionStore`'s current one, so only the *active* page's overlay
 * renders). Double click opens it for full editing, same as before.
 */
function ContinuousPage({
                            page,
                            scale,
                            zIndex,
                            active,
                            registerEl,
                            onSelect,
                            onOpen,
                        }: {
    page: Page
    scale: number
    zIndex: number
    active: boolean
    registerEl: (pageId: string, el: HTMLDivElement | null) => void
    onSelect: (pageId: string) => void
    onOpen: (pageId: string) => void
}) {
    const { t } = useTranslation()
    const wrapperRef = useRef<HTMLDivElement | null>(null)
    const [nearViewport, setNearViewport] = useState(false)

    useEffect(() => {
        const el = wrapperRef.current
        if (!el) return
        // Only fetch/decode a page's image once it's within ~1.5 viewport
        // heights of scrolling into view — with dozens of pages, loading every
        // full-res image up front would be slow and memory-heavy.
        const observer = new IntersectionObserver(
            ([entry]) => {
                if (entry.isIntersecting) setNearViewport(true)
            },
            { rootMargin: '150% 0px' },
        )
        observer.observe(el)
        return () => observer.disconnect()
    }, [])

    const renderedHash = findImageBlob(page, 'rendered')
    const inpaintedHash = findImageBlob(page, 'inpainted')
    const sourceHash = findImageBlob(page, 'source')
    // Prefer the fully rendered page (text baked in); fall back progressively
    // so a page that hasn't been through the pipeline yet still shows *something*.
    const bestHash = renderedHash ?? inpaintedHash ?? sourceHash
    const imageData = useBlobData(nearViewport ? (bestHash ?? undefined) : undefined)

    const width = Math.max(1, Math.round(page.width * scale))
    const height = Math.max(1, Math.round(page.height * scale))

    return (
        <div
            ref={(el) => {
                wrapperRef.current = el
                registerEl(page.id, el)
            }}
            data-testid={`continuous-page-${page.id}`}
            className={`relative shrink-0 cursor-pointer bg-card shadow-sm outline transition-[outline-color] ${
                active ? 'outline-2 outline-primary' : 'outline-1 outline-border/60 hover:outline-primary/50'
            }`}
            style={{ width, height, overflow: 'visible', zIndex }}
            onClick={() => onSelect(page.id)}
            onDoubleClick={() => onOpen(page.id)}
            title={active ? t('view.continuousOpenPage') : t('view.continuousSelectPage')}
        >
            {nearViewport && imageData ? (
                <Image data={imageData} dataKey={bestHash ?? undefined} transition={false} />
            ) : (
                <div className='absolute inset-0 flex items-center justify-center bg-muted/40 text-xs text-muted-foreground'>
                    {nearViewport ? t('view.continuousLoading') : null}
                </div>
            )}
            {active && <TextBlockLayer scale={scale} />}
        </div>
    )
}

export function ContinuousWorkspace() {
    const { t } = useTranslation()
    const { scene } = useScene()
    const scale = useEditorUiStore((s) => s.scale)
    const setContinuousView = useEditorUiStore((s) => s.setContinuousView)
    const currentPageId = useSelectionStore((s) => s.pageId)
    const scaleRatio = scale / 100

    const containerRef = useRef<HTMLDivElement | null>(null)
    const pageElsRef = useRef(new Map<string, HTMLDivElement>())
    const registerEl = (pageId: string, el: HTMLDivElement | null) => {
        if (el) pageElsRef.current.set(pageId, el)
        else pageElsRef.current.delete(pageId)
    }

    const pages = useMemo(() => (scene ? Object.values(scene.pages) : []), [scene])

    // Enabling continuous view should pick up right where you were, not jump
    // back to page 1 — scroll (instantly, no animation) to whichever page was
    // already open. Runs once per mount; `ContinuousWorkspace` only exists
    // while continuous view is on, so mount === "just turned on".
    useEffect(() => {
        if (!currentPageId) return
        const el = pageElsRef.current.get(currentPageId)
        el?.scrollIntoView({ behavior: 'instant' as ScrollBehavior, block: 'start' })
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [])

    const selectPage = (pageId: string) => {
        useSelectionStore.getState().setPage(pageId)
        // Guarantee the text-block overlay is actually clickable/movable: if the
        // user had, say, the brush tool active before switching to continuous
        // view (whose toolbar is hidden here, so they'd have no way to change
        // it), `TextBlockLayer` would otherwise render but ignore all pointer
        // input.
        useEditorUiStore.getState().setMode('select')
    }

    const openPage = (pageId: string) => {
        useSelectionStore.getState().setPage(pageId)
        setContinuousView(false)
    }

    if (pages.length === 0) {
        return (
            <div className='flex h-full w-full items-center justify-center text-sm text-muted-foreground'>
                {t('workspace.importPrompt')}
            </div>
        )
    }

    return (
        <div
            ref={containerRef}
            className='flex w-full flex-col items-center gap-0 py-6'
            data-testid='continuous-workspace'
        >
            {pages.map((page, i) => (
                <ContinuousPage
                    key={page.id}
                    page={page}
                    scale={scaleRatio}
                    // Capped so a long project (dozens/hundreds of pages)
                    // can never produce a z-index higher than any fixed UI
                    // chrome (menus, dialogs, etc). Relative order between
                    // pages near the top of the stack is all that actually
                    // matters for the overflow-painting behavior this
                    // z-index exists for; capping only affects pages far
                    // down the list, which never overlap each other anyway.
                    zIndex={Math.min(pages.length - i, 200)}
                    active={page.id === currentPageId}
                    registerEl={registerEl}
                    onSelect={selectPage}
                    onOpen={openPage}
                />
            ))}
        </div>
    )
}