import { describe, expect, it } from 'vitest'

import { buildStyleBatchOp } from '@/lib/io/textOps'

function sceneWithTwoPages() {
  return {
    pages: {
      p1: {
        id: 'p1',
        nodes: {
          t1: {
            kind: {
              text: {
                style: { fontFamilies: ['Arial'], color: [0, 0, 0, 255], fontSize: 16 },
              },
            },
          },
        },
      },
      p2: {
        id: 'p2',
        nodes: {
          t2: {
            kind: {
              text: {
                style: { fontFamilies: ['Roboto'], color: [1, 2, 3, 255], letterSpacing: 1 },
              },
            },
          },
        },
      },
    },
  } as any
}

describe('buildStyleBatchOp', () => {
  it('applies spacing to every text block on every page without clobbering other style fields', () => {
    const result = buildStyleBatchOp(sceneWithTwoPages(), { lineSpacing: 1.4 }, 'Global spacing update')

    expect(result.affectedPageIds).toEqual(['p1', 'p2'])
    expect(result.affectedNodeCount).toBe(2)
    expect(result.op).toEqual({
      batch: {
        label: 'Global spacing update',
        ops: [
          {
            updateNode: {
              page: 'p1',
              id: 't1',
              patch: {
                data: {
                  text: {
                    style: expect.objectContaining({
                      fontFamilies: ['Arial'],
                      fontSize: 16,
                      lineSpacing: 1.4,
                      letterSpacing: null,
                    }),
                  },
                },
              },
            },
          },
          {
            updateNode: {
              page: 'p2',
              id: 't2',
              patch: {
                data: {
                  text: {
                    style: expect.objectContaining({
                      fontFamilies: ['Roboto'],
                      lineSpacing: 1.4,
                      letterSpacing: 1,
                    }),
                  },
                },
              },
            },
          },
        ],
      },
    })
  })
})
