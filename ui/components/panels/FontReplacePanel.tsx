'use client'

import { useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { Button } from '@/components/ui/button'
import { FontSelect } from '@/components/ui/font-select'
import { Input } from '@/components/ui/input'
import { isTextNode } from '@/hooks/useCurrentPage'
import { useScene } from '@/hooks/useScene'
import {
    getConfig,
    startPipeline,
    useGetGoogleFontsCatalog,
    useListFonts,
} from '@/lib/api/default/default'
import type { FontFaceInfo, NodeDataPatch, Op } from '@/lib/api/schemas'
import { findFontFace, uniqueFontFaces } from '@/lib/font-utils'
import { applyOp } from '@/lib/io/scene'
import { buildTranslationBatchOp, collectAllTextNodes } from '@/lib/io/textOps'
import { ops } from '@/lib/ops'
import { usePreferencesStore } from '@/lib/stores/preferencesStore'

/**
 * "Substituir" tab — swaps one font for another across every text block in
 * the whole document (every loaded page, not just the current one).
 *
 * No backend changes needed: `scene.json` already ships every page to the
 * client, and `Op::Batch` + `Op::UpdateNode` already exist for exactly this
 * kind of multi-node edit, so the whole thing is a client-side scan + one
 * batched op.
 */
export function FontReplacePanel() {
    const { t } = useTranslation()
    const { scene } = useScene()
    const { data: availableFonts = [] } = useListFonts()
    useGetGoogleFontsCatalog() // prefetch catalog so the target picker can decorate Google entries

    const [sourceFont, setSourceFont] = useState('')
    const [targetFont, setTargetFont] = useState('')
    const [isReplacing, setIsReplacing] = useState(false)
    const [replacedCount, setReplacedCount] = useState<number | null>(null)

    // Fonts actually used anywhere in the document — scanned from every page
    // in the scene, not just the one currently open.
    const usedFontFaces = useMemo(() => {
        const seen = new Map<string, FontFaceInfo>()
        if (!scene) return []
        for (const page of Object.values(scene.pages)) {
            for (const node of Object.values(page.nodes)) {
                if (!isTextNode(node)) continue
                const families = node.kind.text.style?.fontFamilies ?? []
                for (const family of families) {
                    const trimmed = family.trim()
                    if (!trimmed || seen.has(trimmed)) continue
                    const known = findFontFace(availableFonts, trimmed)
                    seen.set(
                        trimmed,
                        known ?? {
                            familyName: trimmed,
                            postScriptName: trimmed,
                            source: 'system',
                            cached: true,
                        },
                    )
                }
            }
        }
        return uniqueFontFaces(Array.from(seen.values()))
    }, [scene, availableFonts])

    const targetFontOptions = useMemo(
        () => [...availableFonts].sort((a, b) => a.familyName.localeCompare(b.familyName)),
        [availableFonts],
    )

    const canReplace = !!sourceFont && !!targetFont && sourceFont !== targetFont && !isReplacing

    // ---------------------------------------------------------------------
    // Word / phrase find & replace — independent of the (currently broken,
    // intentionally left alone) font swap above. Same "scan every page, one
    // batched op" approach as the font swap and the global text-case buttons.
    // ---------------------------------------------------------------------
    const [findText, setFindText] = useState('')
    const [replaceText, setReplaceText] = useState('')
    const [isFindReplacing, setIsFindReplacing] = useState(false)
    const [findReplaceResult, setFindReplaceResult] = useState<number | null>(null)

    const canFindReplace = findText.length > 0 && !isFindReplacing

    const handleFindReplace = async () => {
        if (!scene || !canFindReplace) return
        setIsFindReplacing(true)
        setFindReplaceResult(null)
        try {
            const refs = collectAllTextNodes(scene)
            const { op, affectedPageIds, affectedNodeCount } = buildTranslationBatchOp(
                refs,
                (text) => text.split(findText).join(replaceText),
                `Find & replace: "${findText}" -> "${replaceText}"`,
            )
            if (!op) {
                setFindReplaceResult(0)
                return
            }

            await applyOp(op)

            const cfg = await getConfig()
            const renderer = cfg.pipeline?.renderer
            if (renderer && affectedPageIds.length > 0) {
                const defaultFont = usePreferencesStore.getState().defaultFont
                await startPipeline({ steps: [renderer], pages: affectedPageIds, defaultFont })
            }

            setFindReplaceResult(affectedNodeCount)
        } finally {
            setIsFindReplacing(false)
        }
    }

    const handleReplace = async () => {
        if (!scene || !canReplace) return
        setIsReplacing(true)
        setReplacedCount(null)
        try {
            const patchOps: Op[] = []
            const touchedPageIds: string[] = []

            for (const page of Object.values(scene.pages)) {
                let touchedThisPage = false
                for (const [nodeId, node] of Object.entries(page.nodes)) {
                    if (!isTextNode(node)) continue
                    const style = node.kind.text.style
                    if (!style || !style.fontFamilies.includes(sourceFont)) continue

                    const nextFamilies = style.fontFamilies.map((f) => (f === sourceFont ? targetFont : f))
                    patchOps.push(
                        ops.updateNode(page.id, nodeId, {
                            data: { text: { style: { ...style, fontFamilies: nextFamilies } } } as NodeDataPatch,
                        }),
                    )
                    touchedThisPage = true
                }
                if (touchedThisPage) touchedPageIds.push(page.id)
            }

            if (patchOps.length === 0) {
                setReplacedCount(0)
                return
            }

            // One batch op = one undo entry for the whole document-wide swap.
            await applyOp(ops.batch(`Replace font: ${sourceFont} -> ${targetFont}`, patchOps))

            // `queueAutoRender` only tracks a single pending page, so a
            // multi-page replace re-renders every touched page directly instead.
            const cfg = await getConfig()
            const renderer = cfg.pipeline?.renderer
            if (renderer && touchedPageIds.length > 0) {
                const defaultFont = usePreferencesStore.getState().defaultFont
                await startPipeline({ steps: [renderer], pages: touchedPageIds, defaultFont })
            }

            setReplacedCount(patchOps.length)
        } finally {
            setIsReplacing(false)
        }
    }

    return (
        <div className='flex flex-col gap-4 p-2'>
            <div className='flex flex-col gap-3'>
          <span className='text-[10px] font-semibold tracking-wide text-muted-foreground uppercase'>
            {t('fontReplace.sectionFont')}
          </span>
                <div className='flex flex-col gap-1'>
            <span className='text-[10px] font-medium text-muted-foreground uppercase'>
              {t('fontReplace.sourceLabel')}
            </span>
                    <FontSelect
                        value={sourceFont}
                        options={usedFontFaces}
                        placeholder={t('fontReplace.sourcePlaceholder')}
                        onChange={setSourceFont}
                        data-testid='font-replace-source'
                    />
                </div>

                <div className='flex flex-col gap-1'>
            <span className='text-[10px] font-medium text-muted-foreground uppercase'>
              {t('fontReplace.targetLabel')}
            </span>
                    <FontSelect
                        value={targetFont}
                        options={targetFontOptions}
                        placeholder={t('fontReplace.targetPlaceholder')}
                        onChange={setTargetFont}
                        data-testid='font-replace-target'
                    />
                </div>

                <Button
                    type='button'
                    disabled={!canReplace}
                    onClick={() => void handleReplace()}
                    data-testid='font-replace-submit'
                >
                    {isReplacing ? t('fontReplace.replacing') : t('fontReplace.submit')}
                </Button>

                {replacedCount !== null && (
                    <span className='text-xs text-muted-foreground' data-testid='font-replace-result'>
            {replacedCount > 0
                ? t('fontReplace.resultCount', { count: replacedCount })
                : t('fontReplace.resultEmpty')}
          </span>
                )}

                {usedFontFaces.length === 0 && (
                    <span className='text-xs text-muted-foreground'>{t('fontReplace.noFontsUsed')}</span>
                )}
            </div>

            <div className='border-t border-border' />

            <div className='flex flex-col gap-3'>
          <span className='text-[10px] font-semibold tracking-wide text-muted-foreground uppercase'>
            {t('textReplace.title')}
          </span>

                <div className='flex flex-col gap-1'>
            <span className='text-[10px] font-medium text-muted-foreground uppercase'>
              {t('textReplace.findLabel')}
            </span>
                    <Input
                        value={findText}
                        onChange={(e) => setFindText(e.target.value)}
                        placeholder={t('textReplace.findPlaceholder')}
                        className='h-7 text-xs'
                        data-testid='text-replace-find'
                    />
                </div>

                <div className='flex flex-col gap-1'>
            <span className='text-[10px] font-medium text-muted-foreground uppercase'>
              {t('textReplace.replaceLabel')}
            </span>
                    <Input
                        value={replaceText}
                        onChange={(e) => setReplaceText(e.target.value)}
                        placeholder={t('textReplace.replacePlaceholder')}
                        className='h-7 text-xs'
                        data-testid='text-replace-with'
                    />
                </div>

                <Button
                    type='button'
                    disabled={!canFindReplace}
                    onClick={() => void handleFindReplace()}
                    data-testid='text-replace-submit'
                >
                    {isFindReplacing ? t('textReplace.replacing') : t('textReplace.submit')}
                </Button>

                {findReplaceResult !== null && (
                    <span className='text-xs text-muted-foreground' data-testid='text-replace-result'>
            {findReplaceResult > 0
                ? t('textReplace.resultCount', { count: findReplaceResult })
                : t('textReplace.resultEmpty')}
          </span>
                )}
            </div>
        </div>
    )
}