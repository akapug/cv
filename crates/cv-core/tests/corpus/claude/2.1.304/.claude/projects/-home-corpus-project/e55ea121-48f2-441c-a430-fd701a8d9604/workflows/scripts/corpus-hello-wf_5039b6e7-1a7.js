export const meta = {
  name: 'corpus-hello',
  description: 'Two agents each reply with one word',
  phases: [{ title: 'Say' }],
}
const words = await parallel(['ok', 'yes'].map(w => () =>
  agent(`Reply with the single word ${w}. Do not use any tools.`, { label: `say:${w}`, phase: 'Say' })))
return { words }
