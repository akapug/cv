// cv-compact: cv's compaction inside Claude Code's own (the cv 0.19 precompact handoff).
//
// On every compaction of the main conversation (`session.compact`, trigger `auto`, `manual` or
// `plugin`), a seat that serves cv's runs `cv watch --kind precompact commit` with the
// conversation Claude Code is about to compact. On a pass, the summary replaces the messages
// before `keep_from` and the rest are kept as Claude Code holds them (by handle). On any miss, an
// error or the deadline, Claude Code compacts as its own (`next(e)`, its native precompute
// included). Every compaction, cv's or Claude Code's, is recorded with
// `cv watch --kind precompact observe`, so `cv watch --kind precompact report` compares them.
//
// The `precompute` trigger: a seat that serves cv's asks cv first (`commit --check`). When cv
// would serve right now the background summary is vetoed (`{ skip }`, which also holds it off
// until the next real prompt, when it is asked again): the compaction need not wait out a summary
// it throws away, or pay for it. When cv would not serve, it runs and stays the miss path's fast
// answer. No proxy sits in the path; cv is never waited on past the deadline.
import type { Register, SessionCompactInput, SessionCompactResult, SessionMessage } from 'claude-code'

type Commit = { outcome: 'served'; summary: string; keep_from: number }

// What the commit reads of a message: its text, its tool call and result ids, and its size. A
// tool's output is counted once, on the message that holds its result; a call's `text` repeats it.
const slim = (m: SessionMessage) => ({
  role: m.role,
  text: m.text,
  toolUses: m.toolUses.map((u) => ({ tool_use_id: u.tool_use_id, tool: u.tool })),
  toolResults: (m.toolResults ?? []).map((r) => ({ tool_use_id: r.tool_use_id })),
  bytes:
    m.text.length +
    m.toolUses.reduce((n, u) => n + JSON.stringify(u.input ?? {}).length, 0) +
    (m.toolResults ?? []).reduce((n, r) => n + (r.text?.length ?? 0), 0),
})

export const register: Register = (on, options) => {
  const cv = typeof options.cv === 'string' && options.cv !== '' ? options.cv : 'cv'
  const serve = typeof options.serve === 'string' ? options.serve : 'host'
  const deadline = typeof options.deadline_ms === 'number' ? options.deadline_ms : 5000
  const precompact = [cv, 'watch', '--kind', 'precompact']

  on('session.compact', async ($, e: SessionCompactInput, next): Promise<SessionCompactResult> => {
    if (e.agentId !== undefined) return next(e)
    const session = await $.session.id()
    const serving = serve === 'cv' ? ['--serve', 'cv'] : []
    if (e.trigger === 'precompute') {
      if (serve === 'native') return next(e)
      const input = JSON.stringify({ session, harness: 'claude', trigger: e.trigger, messages: e.messages.map(slim) })
      const asked = await $.process
        .run([...precompact, 'commit', '--check', ...serving], { stdin: input, timeoutMs: deadline })
        .catch(() => undefined)
      return asked?.exitCode === 0 ? { skip: 'cv holds a ready summary for this compaction' } : next(e)
    }
    const started = await $.clock.now()
    let miss: string | undefined
    let result: SessionCompactResult | undefined
    let method = 'native'
    if (serve !== 'native') {
      const argv = [...precompact, 'commit', ...serving]
      const input = JSON.stringify({
        session,
        harness: 'claude',
        trigger: e.trigger,
        instructions: e.instructions,
        messages: e.messages.map(slim),
      })
      try {
        const run = await $.process.run(argv, { stdin: input, timeoutMs: deadline })
        if (run.exitCode === 0) {
          const c = JSON.parse(run.stdout) as Commit
          const kept = e.messages.slice(c.keep_from)
          result = { messages: [{ role: 'user', text: c.summary, toolUses: [] }, ...kept] }
          method = 'cv'
        } else {
          miss = run.stderr.trim().split('\n').pop() || `exit ${run.exitCode}`
        }
      } catch (err) {
        miss = `cv commit did not answer: ${String(err)}`
      }
    }
    if (result === undefined) result = await next(e)
    const ms = (await $.clock.now()) - started
    if (result.messages !== undefined) {
      const summary = result.messages.find((m) => m.handle === undefined)
      const row = {
        session,
        trigger: e.trigger,
        method,
        ms,
        messages_before: e.messages.length,
        messages_after: result.messages.length,
        tokens_before: result.tokensBefore,
        tokens_after: result.tokensAfter,
        summary_bytes: summary?.text.length,
        miss,
      }
      // Recorded after the compaction stands, never awaited by it.
      void $.process.run([...precompact, 'observe'], { stdin: JSON.stringify(row), timeoutMs: 10000 }).catch(() => {})
    }
    return result
  }).catch(($, e, next) => next(e)) // a hook that throws: Claude Code compacts as its own
}
