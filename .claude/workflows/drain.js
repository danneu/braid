export const meta = {
  name: 'drain',
  description: 'Implement the decided review findings, a few at a time in dependency order, until nothing is runnable',
  whenToUse: 'After /pick-fix records decisions, or after answering an ask. args: {max?: 3}',
  phases: [
    { title: 'Queue', detail: 'read the store before every start' },
    { title: 'Implement', detail: 'one implement run per runnable finding, at most max at once' },
  ],
}

// plan/2026-10-21-1424/D5. Decisions are recorded by /pick-fix; this runs
// them unattended. Which findings may start is decided by
// `findings.py queue` (decided, every dependency merged, not named by an
// open ask), read again before every start so decisions and answers made
// mid-drain are picked up. Each finding runs at most once per drain: a run
// that ends without merging (an ask, a block, a crash) waits for the user.

const opts = args || {}
const max = opts.max || 3
if (!Number.isInteger(max) || max < 1) throw new Error('args.max must be a positive integer')
// Overridable so a scratch repository and script can stand in for the real ones in tests.
const F = opts.findings || 'python3 scripts/findings/findings.py'
const IMPLEMENT = opts.implement ? { scriptPath: opts.implement } : 'implement'

const RAN = {
  type: 'object',
  properties: { stdout: { type: 'string' }, exit_code: { type: 'integer' } },
  required: ['stdout', 'exit_code'],
}
// The store returns ids only (plan/2026-10-21-1424/D1); the shape is checked
// so a garbled relay stops the drain instead of starting the wrong work.
const LISTS = ['runnable', 'waiting', 'held', 'asking', 'blocked']
async function queue() {
  const r = await agent(
    `Run this exact command once with the Bash tool and return its stdout verbatim and its exit code. Do nothing else.\n\n${F} queue`,
    { label: 'queue', phase: 'Queue', model: 'haiku', effort: 'low', schema: RAN },
  )
  if (!r) throw new Error('queue: runner failed')
  if (r.exit_code !== 0) throw new Error(`queue: ${r.stdout}`)
  let q
  try { q = JSON.parse(r.stdout) } catch (e) { throw new Error(`queue: not JSON: ${r.stdout}`) }
  const ok = q && LISTS.every(k => Array.isArray(q[k]) && q[k].every(x => typeof x === 'string'))
  if (!ok) throw new Error(`queue: malformed store result: ${r.stdout}`)
  return q
}

const inflight = new Map()
const ran = new Set()
const results = []
const events = []
let peak = 0

while (true) {
  phase('Queue')
  const q = await queue()
  const next = q.runnable.filter(id => !ran.has(id) && !inflight.has(id))
  for (const id of next.slice(0, max - inflight.size)) {
    ran.add(id)
    events.push(`start ${id}`)
    log(`start ${id} (${inflight.size + 1} of at most ${max} running)`)
    const run = workflow(IMPLEMENT, { id, findings: opts.findings, notify: opts.notify })
      .then(r => r || { id, status: 'gap', reason: 'implement returned nothing' })
      .catch(e => ({ id, status: 'gap', reason: String(e && e.message || e) }))
      .then(r => ({ id, r }))
    inflight.set(id, run)
  }
  peak = Math.max(peak, inflight.size)
  if (inflight.size === 0) {
    // Nothing started this round and nothing is running: q is the final state.
    const left = q
    log(`drain done: ${results.filter(r => r.status === 'merged').length} merged`)
    return {
      merged: results.filter(r => r.status === 'merged').map(r => ({ id: r.id, commit: r.commit || null })),
      asking: results.filter(r => r.status === 'asking').map(r => ({ id: r.id, question: r.reason })),
      blocked: results.filter(r => r.status === 'blocked').map(r => ({ id: r.id, reason: r.reason })),
      unfinished: results.filter(r => !['merged', 'asking', 'blocked'].includes(r.status))
        .map(r => ({ id: r.id, status: r.status, reason: r.reason || '' })),
      // Still in the store, untouched by this drain.
      waiting_on_dependencies: left.waiting,
      held_by_asks: left.held,
      open_asks: left.asking,
      open_blocks: left.blocked,
      peak_in_flight: peak,
      events,
    }
  }
  phase('Implement')
  const done = await Promise.race(inflight.values())
  inflight.delete(done.id)
  events.push(`end ${done.id} ${done.r.status}`)
  log(`${done.id}: ${done.r.status}${done.r.reason ? ` (${done.r.reason})` : ''}`)
  results.push({ id: done.id, ...done.r })
}
