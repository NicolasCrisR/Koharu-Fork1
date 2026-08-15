// Portuguese/Spanish/English-style casing helpers. Only meaningful for
// scripts with letter case (Latin, Cyrillic, Greek...); harmless no-ops on
// CJK output since those scripts have no case to change.
export const toLowerCaseText = (s: string) => s.toLowerCase()
export const toUpperCaseText = (s: string) => s.toUpperCase()
export const toSentenceCaseText = (s: string) =>
    s.toLowerCase().replace(/(^\s*\S|[.!?]\s+\S)/g, (m) => m.toUpperCase())
export const toTitleCaseText = (s: string) =>
    s.toLowerCase().replace(/\b\p{L}/gu, (m) => m.toUpperCase())

export type TextCaseKey = 'lower' | 'upper' | 'sentence' | 'title'

export const TEXT_CASE_TRANSFORMS: {
    key: TextCaseKey
    label: string
    titleKey: string
    fn: (s: string) => string
}[] = [
    { key: 'lower', label: 'aa', titleKey: 'render.textCaseLower', fn: toLowerCaseText },
    { key: 'upper', label: 'AA', titleKey: 'render.textCaseUpper', fn: toUpperCaseText },
    { key: 'sentence', label: 'Aa', titleKey: 'render.textCaseSentence', fn: toSentenceCaseText },
    { key: 'title', label: 'Aa Aa', titleKey: 'render.textCaseTitle', fn: toTitleCaseText },
]