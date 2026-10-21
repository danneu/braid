export const meta = {
  name: 'review',
  description: 'Review braid slices, verify and merge findings, and pitch fixes into the finding store',
  whenToUse: 'Find ranked, verified, pitched improvements. args: {slices?: ["command:add", "docs:*", ...], focus?: "text", run?: "YYMMDD-HHMM"}',
  phases: [
    { title: 'Discover', detail: 'open a run and list slices' },
    { title: 'Review', detail: 'one reviewer per slice' },
    { title: 'Verify', detail: 'one skeptic per finding' },
    { title: 'Merge', detail: 'collapse duplicates, set dependencies' },
    { title: 'Pitch', detail: 'ranked solutions per finding' },
    { title: 'Critique', detail: 'one critic per pitch; one revision when asked' },
  ],
}

// Steps 1-4 of plan/2026-10-21-1047, plus the pitch critic of
// plan/2026-10-21-1424/D3. Agents record their results through
// scripts/findings/findings.py, which validates every payload and owns all
// state; this script only routes work and accounts for coverage. A slice or
// finding whose agent fails stays where it was, and the return value names
// every gap (plan/2026-10-21-1047/I7).

const FINDINGS = 'python3 scripts/findings/findings.py'
const READ_ONLY = 'Never modify tracked files or git state. Write to the finding store only through ' +
  `\`${FINDINGS}\`; on \`error: ...\`, fix the payload and call it again.`

// Lenses, not discovered: each is described in .claude/agents/reviewer.md.
const CROSS = ['module', 'parsers', 'subprocess', 'dry-run', 'tests', 'architecture', 'performance']

const opts = args || {}
const focus = opts.focus ? `\n\nFocus for this run (apply it on top of your lens):\n${opts.focus}` : ''

const SLICE = {
  type: 'object',
  properties: {
    id: { type: 'string', description: 'command:<name with spaces as hyphens> or docs:<path under docs/>' },
    files: { type: 'array', items: { type: 'string' } },
  },
  required: ['id', 'files'],
}

phase('Discover')
const found = await agent(
  `Prepare a braid review run.

1. ${opts.run ? `Use the existing run ${opts.run}; do not create one.` : `Run \`${FINDINGS} new-run\` and take the run id it prints. If it fails (another run started this minute), never reuse an existing run: return an empty run id and no slices.`}
2. List every public leaf command of the braid CLI from the clap definitions reachable from \`cli/src/main.rs\`, using effective clap names. Skip hidden commands and parent-only commands (\`ups\` is a parent; \`ups status\` is a leaf). Slice id: \`command:<name>\` with spaces as hyphens, e.g. \`command:ups-status\`. Files: the dispatch site, the command module, and its \`docs/commands/\` page.
3. List every page linked from \`docs/SUMMARY.md\`. Slice id: \`docs:<path relative to docs/>\`, e.g. \`docs:guides/auto-unlock.md\`. Files: the page itself.

Read only; change nothing. Return the run id and the slices.`,
  {
    label: 'discover', phase: 'Discover', effort: 'low',
    schema: {
      type: 'object',
      properties: { run: { type: 'string' }, slices: { type: 'array', items: SLICE } },
      required: ['run', 'slices'],
    },
  },
)
if (!found || !found.run) throw new Error('discovery failed or no new run could be created; nothing was reviewed')
const run = found.run

const all = [
  ...found.slices,
  ...CROSS.map(c => ({ id: `cross:${c}`, files: [] })),
]
const wanted = opts.slices && opts.slices.length ? opts.slices : null
const matches = (id, pat) => pat.endsWith('*') ? id.startsWith(pat.slice(0, -1)) : id === pat
const slices = wanted ? all.filter(s => wanted.some(p => matches(s.id, p))) : all
const unmatched = wanted ? wanted.filter(p => !all.some(s => matches(s.id, p))) : []
if (unmatched.length) log(`no slice matches ${unmatched.join(', ')}`)
log(`run ${run}: reviewing ${slices.length} of ${all.length} slices`)

const REVIEWED = {
  type: 'object',
  properties: {
    slice: { type: 'string' },
    ids: { type: 'array', items: { type: 'string' }, description: 'ids the ingest call printed' },
    note: { type: 'string' },
  },
  required: ['slice', 'ids', 'note'],
}
const VERDICT = {
  type: 'object',
  properties: {
    id: { type: 'string' },
    verdict: { enum: ['verified', 'refuted'] },
    reason: { type: 'string' },
  },
  required: ['id', 'verdict', 'reason'],
}

const verifyPrompt = id => `You are a skeptic. Try to refute braid review finding ${id}.

Read it with \`${FINDINGS} show ${id}\`. Then check the claim against the code, tests, and \`reference/\` yourself; do not trust its evidence. Refute it if the claim is false, already fixed, impossible to trigger, contradicted by a documented decision in \`docs/design/\`, or not worth anyone's time. A claim you cannot confirm from the code is refuted. If it holds, it is verified.

Record the verdict:
\`\`\`sh
${FINDINGS} verify --input - <<'EOF'
[{"id": "${id}", "verdict": "verified|refuted", "reason": "<what you checked and what you found>"}]
EOF
\`\`\`
${READ_ONLY} Return the verdict you recorded.`

phase('Review')
const reviewed = await pipeline(
  slices,
  // The reviewer agent type has no Write or Edit tools, which backs
  // plan/2026-10-21-1047/I5 with more than an instruction.
  s => agent(
    `Run id: ${run}\nSlice: ${s.id}\nStarting files:\n${s.files.map(f => `- ${f}`).join('\n') || '- (find them yourself)'}${focus}`,
    { label: `review:${s.id}`, phase: 'Review', agentType: 'reviewer', schema: REVIEWED },
  ),
  r => parallel((r ? r.ids : []).map(id => () =>
    agent(verifyPrompt(id), { label: `verify:${id}`, phase: 'Verify', effort: 'high', schema: VERDICT }),
  )).then(verdicts => ({ review: r, verdicts })),
)

const gaps = []
const verified = []
reviewed.forEach((r, i) => {
  if (!r || !r.review) { gaps.push(`review failed: ${slices[i].id}`); return }
  r.review.ids.forEach((id, j) => {
    const v = r.verdicts[j]
    if (!v) gaps.push(`verify failed: ${id} (still open)`)
    else if (v.verdict === 'verified') verified.push(id)
  })
})
log(`${verified.length} verified finding(s); ${gaps.length} gap(s)`)

// Barrier: duplicates are only visible across every slice's findings.
let removed = []
if (verified.length > 1) {
  phase('Merge')
  const merged = await agent(
    `Deduplicate the verified findings of braid review run ${run}.

List them with \`${FINDINGS} list ${run} --state verified\` and read each with \`${FINDINGS} show <id>\`. Two findings are duplicates when one fix resolves both. Keep the one with the clearest claim and absorb the rest. Separately, record a dependency when one finding's fix must land before another's can.

Record both in one call (omit a key you do not need; skip the call if there is nothing to record):
\`\`\`sh
${FINDINGS} merge --input - <<'EOF'
{"groups": [{"keep": "<id>", "absorb": ["<id>", ...]}], "depends_on": {"<id>": ["<id it needs first>"]}}
EOF
\`\`\`
${READ_ONLY} Return the ids the script removed.`,
    {
      label: 'merge', phase: 'Merge', effort: 'high',
      schema: { type: 'object', properties: { removed: { type: 'array', items: { type: 'string' } } }, required: ['removed'] },
    },
  )
  if (merged) removed = merged.removed
  else gaps.push('merge failed: duplicates may remain')
}
const toPitch = verified.filter(id => !removed.includes(id))

const pitchPrompt = id => `Pitch fixes for braid review finding ${id} so the user can choose one.

Read it with \`${FINDINGS} show ${id}\`, then the code it cites, \`docs/design/principles.md\`, and the relevant ADRs.

Follow the design bar in AGENTS.md. First work out the ideal fix: the simplest structure in which the problem cannot happen, ignoring scope, churn, and compatibility. Then list the candidates worth choosing between, best design first; never rank by effort. Include the ideal even if you recommend something else, and recommend something else only by naming a constraint the ideal breaks, a behavior it loses, or a risk it cannot remove. If the honest best move is to leave the code alone, make that a candidate. List at least one rejected solution with the reason it lost.

Every candidate's \`key\` is kebab-case. Its \`summary\` says what changes, \`tradeoffs\` what it costs and gains, and \`worst_case\` the worst realistic outcome if it goes wrong. Record it:
\`\`\`sh
${FINDINGS} pitch --input - <<'EOF'
[{"id": "${id}", "issue": "<the problem in plain words>", "candidates": [{"key": "...", "title": "...", "summary": "...", "tradeoffs": "...", "worst_case": "..."}], "ideal": "<candidate key>", "rejected": [{"title": "...", "reason": "..."}], "recommendation": {"key": "<candidate key>", "justification": "..."}}]
EOF
\`\`\`
${READ_ONLY} Return the recommended key.`

phase('Pitch')
const pitched = await parallel(toPitch.map(id => () =>
  agent(pitchPrompt(id), {
    label: `pitch:${id}`, phase: 'Pitch', effort: 'xhigh',
    schema: { type: 'object', properties: { recommendation: { type: 'string' } }, required: ['recommendation'] },
  }),
))
pitched.forEach((p, i) => { if (!p) gaps.push(`pitch failed: ${toPitch[i]} (still verified)`) })
const pitchedIds = toPitch.filter((_, i) => pitched[i])

// Every pitch faces a critic before the user sees it (plan/2026-10-21-1424/D3).
// A barrier: each critic reads every other pitch in the run, to catch
// conflicts and ordering.
const critiquePrompt = id => `You are a critic. Attack the pitch for braid review finding ${id} before the user sees it.

Read it with \`${FINDINGS} show ${id}\`, the code it cites, \`docs/design/principles.md\`, and the relevant ADRs. Then read every other pitch in this run: \`${FINDINGS} list ${run} --state pitched\`, and \`${FINDINGS} show <id>\` for each.

Attack these points, and only these:
- the ideal is not the simplest structure in which the problem cannot happen (AGENTS.md design bar);
- a candidate worth choosing is missing;
- the ranking or the recommendation does not follow from the stated trade-offs;
- the pitch conflicts with another pitch in the run (one's fix undoes, contradicts, or already covers the other's);
- an ordering is needed: this fix must land after another finding's, or before it.

Record every ordering you find as a dependency (the finding that must land first goes in \`--add\`):
\`\`\`sh
${FINDINGS} depends <finding that waits> --add <finding that lands first>
\`\`\`
Then record your verdict: \`sound\` if nothing above needs the pitch changed, else \`revise\` with one concrete point per problem. A dependency alone is not a reason to revise.
\`\`\`sh
${FINDINGS} critique --input - <<'EOF'
[{"id": "${id}", "verdict": "sound|revise", "points": ["<concrete problem and what the pitch should say instead>"]}]
EOF
\`\`\`
${READ_ONLY} Return the verdict.`

const reviseNote = id => `\n\nThis finding is already pitched, and a critic asked for one revision. Read its points under Critique in \`${FINDINGS} show ${id}\`. Address each point, or keep your position where a point is wrong and say why in the pitch's text. Record the full revised pitch with the same command; this is the only revision.`

let critiqued = []
if (pitchedIds.length) {
  phase('Critique')
  const verdicts = await parallel(pitchedIds.map(id => () =>
    agent(critiquePrompt(id), {
      label: `critique:${id}`, phase: 'Critique', effort: 'xhigh',
      schema: { type: 'object', properties: { verdict: { enum: ['sound', 'revise'] } }, required: ['verdict'] },
    }),
  ))
  verdicts.forEach((v, i) => { if (!v) gaps.push(`critique failed: ${pitchedIds[i]} (pitched, not critiqued)`) })
  critiqued = pitchedIds.filter((_, i) => verdicts[i])
  const toRevise = pitchedIds.filter((_, i) => verdicts[i] && verdicts[i].verdict === 'revise')
  const revised = await parallel(toRevise.map(id => () =>
    agent(pitchPrompt(id) + reviseNote(id), {
      label: `revise:${id}`, phase: 'Critique', effort: 'xhigh',
      schema: { type: 'object', properties: { recommendation: { type: 'string' } }, required: ['recommendation'] },
    }),
  ))
  revised.forEach((r, i) => { if (!r) gaps.push(`revision failed: ${toRevise[i]} (critique stands, pitch unrevised)`) })
}

gaps.forEach(g => log(`gap: ${g}`))
return {
  run,
  index: `findings/${run}/index.md`,
  slices: slices.map(s => s.id),
  pitched: pitchedIds,
  critiqued,
  coverage: gaps.length ? 'partial' : 'complete',
  gaps,
}
