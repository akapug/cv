import { expect, test } from 'claude-code/testing'
import type { On, SessionCompactArgs } from 'claude-code'

const conversation = [
  { role: 'user', text: 'please refactor the parser module', toolUses: [] },
  { role: 'assistant', text: 'I will refactor the parser module now.', toolUses: [] },
  { role: 'user', text: 'and the writer', toolUses: [] },
] as const

// The engine beneath the plugin: its session, its clock, its compaction, and the cv it runs.
function engine(on: On, cv: { exitCode: number; stdout: string; stderr: string }) {
  const runs: string[][] = []
  const native = { compacted: 0 }
  on('session.id', () => ({ value: 's-1' }))
  on('clock.now', () => ({ value: 1000 }))
  on('session.messages', () => ({ value: conversation }))
  on('process.run', (_, e) => {
    runs.push([...e.argv])
    return { value: e.argv.includes('commit') ? cv : { exitCode: 0, stdout: '', stderr: '' } }
  })
  on('session.compact', () => {
    native.compacted += 1
    return { messages: [{ role: 'user', text: 'the native summary', toolUses: [] }], tokensBefore: 900, tokensAfter: 90 }
  })
  return { runs, native }
}

// A test raises the event with its whole input, as the engine would.
const auto = { trigger: 'auto', messages: conversation } as unknown as SessionCompactArgs

const served = { exitCode: 0, stdout: JSON.stringify({ outcome: 'served', summary: 'cv summary', keep_from: 2 }), stderr: '' }
const missed = { exitCode: 3, stdout: '', stderr: 'precompact commit: miss (no_record): no ready summary for this session' }

test('a seat serving cv takes the ready summary and keeps the messages after the cut', { options: { serve: 'cv' } }, async ($, on) => {
  const { runs, native } = engine(on, served)
  const result = await $.session.compact(auto)
  expect(native.compacted).toBe(0)
  expect(result.messages?.[0]?.text).toBe('cv summary')
  expect(result.messages?.length).toBe(1 + conversation.length - 2)
  expect(runs[0]).toEqual(['cv', 'watch', '--kind', 'precompact', 'commit', '--serve', 'cv'])
  expect(runs.some((argv) => argv.includes('observe'))).toBe(true)
})

test('a miss compacts natively and is recorded', { options: { serve: 'cv' } }, async ($, on) => {
  const { runs, native } = engine(on, missed)
  const result = await $.session.compact(auto)
  expect(native.compacted).toBe(1)
  expect(result.messages?.[0]?.text).toBe('the native summary')
  expect(runs.some((argv) => argv.includes('observe'))).toBe(true)
})

test('a seat serving native never asks cv to commit', { options: { serve: 'native' } }, async ($, on) => {
  const { runs, native } = engine(on, served)
  await $.session.compact(auto)
  expect(native.compacted).toBe(1)
  expect(runs.some((argv) => argv.includes('commit'))).toBe(false)
})
