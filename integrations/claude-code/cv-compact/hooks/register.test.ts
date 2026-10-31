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
  const stdins: string[] = []
  const native = { compacted: 0 }
  on('session.id', () => ({ value: 's-1' }))
  on('clock.now', () => ({ value: 1000 }))
  on('session.messages', () => ({ value: conversation }))
  on('process.run', (_, e) => {
    runs.push([...e.argv])
    stdins.push(String(e.init?.stdin))
    return { value: e.argv.includes('commit') ? cv : { exitCode: 0, stdout: '', stderr: '' } }
  })
  on('session.compact', () => {
    native.compacted += 1
    return { messages: [{ role: 'user', text: 'the native summary', toolUses: [] }], tokensBefore: 900, tokensAfter: 90 }
  })
  return { runs, stdins, native }
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

test('a miss with nothing on stderr still names why', { options: { serve: 'cv' } }, async ($, on) => {
  const { runs, stdins, native } = engine(on, { exitCode: 7, stdout: '', stderr: '' })
  await $.session.compact(auto)
  expect(native.compacted).toBe(1)
  const observed = runs.findIndex((argv) => argv.includes('observe'))
  expect(JSON.parse(stdins[observed] ?? '{}').miss).toBe('exit 7')
})

test('a tool result is counted once in a message size', { options: { serve: 'cv' } }, async ($, on) => {
  const { stdins } = engine(on, missed)
  const call = { tool_use_id: 't', tool: 'Bash', input: {}, text: 'x'.repeat(100) }
  const result = { tool_use_id: 't', text: 'x'.repeat(100), isError: false }
  const messages = [
    { role: 'assistant', text: '', toolUses: [call] },
    { role: 'user', text: '', toolUses: [], toolResults: [result] },
  ]
  await $.session.compact({ trigger: 'auto', messages } as unknown as SessionCompactArgs)
  const sizes = JSON.parse(stdins[0] ?? '{}').messages.map((m: { bytes: number }) => m.bytes)
  expect(sizes).toEqual([2, 100])
})
