export const meta = {
  name: 'implement',
  description: 'Implement one decided finding in its own worktree, gate and vet it, and land it on master',
  whenToUse: 'Started by the drain workflow, one per runnable finding. args: {id: "YYMMDD-HHMM-NNN"}',
  phases: [
    { title: 'Launch', detail: 'read the decision; stop, resume, or start' },
    { title: 'Implement', detail: 'apply the chosen solution with its proof' },
    { title: 'Gate', detail: 'acceptance recipes and the proof, on a snapshot of the candidate' },
    { title: 'Vet', detail: 'a fresh reviewer approves the tree and the message that land' },
    { title: 'Land', detail: 'merge queue: rebase, re-gate if changed, fast-forward' },
  ],
}

// Step 6 of plan/2026-10-21-1047. Every decision that matters (may this run
// act, which branch, may this tree land) is made by
// scripts/findings/findings.py; agents only do the work and run commands.
// A crash anywhere leaves the finding decided with its branch, and a
// relaunch resumes there (plan/2026-10-21-1047/D12).

const opts = args || {}
if (!opts.id) throw new Error('args.id is required')
const id = opts.id
// Overridable so a scratch repository can stand in for the real one in tests,
// without notifying the user (`notify: false`).
const F = opts.findings || 'python3 scripts/findings/findings.py'
const MAX_REPAIRS = 2
const MAX_LAND_WAITS = 12

const RAN = {
  type: 'object',
  properties: { stdout: { type: 'string' }, stderr: { type: 'string' } },
  required: ['stdout', 'stderr'],
}
async function sh(label, command, phaseName) {
  const r = await agent(
    `Run this exact command once with the Bash tool, with a 600000 ms timeout. Return its stdout and its stderr exactly as printed, each an empty string if there was none. Add no notes or commentary to either. Do nothing else.\n\n${command}`,
    { label, phase: phaseName, model: 'haiku', effort: 'low', schema: RAN },
  )
  if (!r) throw new Error(`${label}: runner failed`)
  return r
}
// The store returns only short control fields (plan/2026-10-21-1424/D1), and
// each result is checked against its expected shape before use, so a relay
// that drops or garbles a field stops the run instead of steering it.
const TYPES = {
  string: v => typeof v === 'string',
  integer: v => Number.isInteger(v),
  boolean: v => typeof v === 'boolean',
  'string?': v => v === null || typeof v === 'string',
  'string[]': v => Array.isArray(v) && v.every(x => typeof x === 'string'),
}
function expect(label, value, shape) {
  const bad = Object.entries(shape).filter(([k, t]) => !(value && TYPES[t](value[k])))
  if (bad.length) throw new Error(`${label}: malformed store result (${bad.map(([k, t]) => `${k}: ${t}`).join(', ')}): ${JSON.stringify(value)}`)
  return value
}
// The store prints a JSON document on stdout only when it succeeds; an error
// goes to stderr alone. So the document is the success signal, not the exit
// code, which a runner cannot always see and sometimes invents. Runners also
// add notes despite being told not to, so take the document out of the text.
function jsonBlock(text) {
  const lines = text.split('\n')
  for (let s = 0; s < lines.length; s++) {
    if (!/^[[{]/.test(lines[s])) continue
    for (let e = s; e < lines.length; e++) {
      if (!/^[\]}]/.test(lines[e]) && e !== s) continue
      try { return JSON.parse(lines.slice(s, e + 1).join('\n')) } catch (err) { /* keep looking */ }
    }
  }
  return undefined
}
async function store(label, command, phaseName, shapeOf) {
  const r = await sh(label, command, phaseName)
  const value = jsonBlock(r.stdout)
  if (value === undefined) throw new Error(`${label}: store failed: ${r.stderr || r.stdout}`)
  return expect(label, value, shapeOf(value))
}

// The extra fields each `land` status carries.
const LAND = {
  merged: {},
  wait: { reason: 'string' },
  regate: { tree: 'string' },
  conflict: { files: 'string[]' },
}

phase('Launch')
const plan = await store('launch', `${F} launch ${id}`, 'Launch', v =>
  v && v.action === 'stop'
    ? { id: 'string', action: 'string', reason: 'string' }
    : { id: 'string', action: 'string', branch: 'string', decision: 'integer', candidate: 'string?', proof: 'string', acceptance: 'string[]' })
if (plan.id !== id || !['stop', 'start', 'resume'].includes(plan.action)) throw new Error(`launch: unexpected result ${JSON.stringify(plan)}`)
if (plan.action === 'stop') {
  log(`${id}: ${plan.reason}; nothing to do`)
  return { id, status: 'stopped', reason: plan.reason }
}
const wt = await store('worktree', `${F} worktree ${id}`, 'Launch',
  () => ({ id: 'string', branch: 'string', path: 'string', created: 'boolean' }))
if (wt.branch !== plan.branch) throw new Error(`worktree: branch ${wt.branch} is not ${plan.branch}`)
log(`${id}: ${plan.action} on ${wt.branch} in ${wt.path}`)

const regression = plan.proof === 'regression'
// Agents read the decision's text from the store themselves (plan/2026-10-21-1424/I10).
const decision = `Decision ${plan.decision} in \`${F} show ${id}\`: ${plan.candidate
  ? `candidate \`${plan.candidate}\` from the pitch, with the trade-offs it lists as accepted, plus the decision's note if it has one`
  : 'no candidate; the decision\'s note is the whole direction'}.`
const acceptance = plan.acceptance.map(r => `just ${r}`)

const IMPL = {
  type: 'object',
  properties: {
    status: { enum: ['done', 'ask', 'blocked'] },
    reason: { type: 'string', description: 'one line: the question asked, why blocked, or a summary of the change' },
    proof_command: { type: 'string', description: 'the command that runs the proof check' },
    subject: { type: 'string', description: 'conventional commit subject, lowercase' },
    body: { type: 'string', description: 'why: what led to the change, how it was chosen' },
  },
  required: ['status', 'reason', 'proof_command', 'subject', 'body'],
}
const GATE = {
  type: 'object',
  properties: {
    tree: { type: 'string' },
    passed: { type: 'boolean' },
    report: { type: 'string', description: 'each check, its result, and the failing output' },
  },
  required: ['tree', 'passed', 'report'],
}
const VET = {
  type: 'object',
  properties: {
    tree: { type: 'string' },
    approved: { type: 'boolean' },
    problems: { type: 'array', items: { type: 'string' } },
    message: { type: 'string', description: 'the exact commit message to land: subject, blank line, body' },
  },
  required: ['tree', 'approved', 'problems', 'message'],
}

// A command returns when it exits; polling for its exit can stop early or hang.
const RUN_RULE = 'Run each command in the foreground with the longest Bash timeout and let it return when it exits. If it may outlast that timeout (VM tests can), start it with run_in_background instead and wait for the automatic notification that it exited. Never wait by checking on a process or its output: no pgrep, ps, tail, grep, until or while loops, or sleep.'

// Runs ask instead of guessing (plan/2026-10-21-1424/D6): the implementer
// records the ask itself, so its text reaches the store without a relay.
const ASK_RULES = `Stop and ask the user when you learn something that would change their call:
- the chosen solution is impossible, or clearly worse than another;
- a trade-off the user accepted proves materially different;
- the work needs a choice the decision does not cover that changes observable behavior, an invariant, or an ADR;
- it affects another queued finding (\`${F} list <run> --state decided\` lists them).
Anything with an obvious answer inside the decision, decide yourself and say so in the landing message body. To ask, leave the branch committed and clean, record it, and return status \`ask\` with the question in one line:
\`\`\`sh
${F} ask ${id} --input - <<'EOF'
{"found": "<what you learned and why it matters>", "options": [{"key": "<kebab-case>", "title": "...", "summary": "<what changes>", "tradeoffs": "..."}, ...], "recommendation": {"key": "<option key>", "justification": "..."}, "affects": ["<ids of other findings it affects>"]}
EOF
\`\`\`
Return status \`blocked\` with the reason only for a failure that needs no choice, such as broken tooling.`

const workRules = (feedback, prior) => `First check the decision against \`master\` as it is now (plan/2026-10-21-1424/D9): other findings may have landed since the pitch. Compare the finding's claim and the pitch's reasoning with the current code. An adaptation that keeps the chosen solution is yours to make; say what changed in the landing message body. One that changes the solution is an ask.

${ASK_RULES}

Rules:
- Apply exactly this solution; never swap in another one on your own.
- Follow AGENTS.md: principles, ADRs, doc comments, docs in sync, test preambles.
${regression
    ? `- Proof (regression): the finding's Proof section says what the test must show.\n  First commit only the test, and confirm it fails for the right reason. Give that commit a \`Proof: red\` trailer. Then commit the fix and confirm the test passes. Keep exactly one \`Proof: red\` commit on the branch.`
    : `- Proof (${plan.proof}): the finding's Proof section says what must hold.`}
- Make every listed acceptance recipe pass: ${acceptance.join(', ')}. VM recipes take a host-wide gate and may wait. ${RUN_RULE}
- Commit all work on the branch (stage files by name; never --amend, never --no-verify, never push, never touch master). Leave the worktree clean. Branch commit messages never land; only the message you return does.
${prior ? `\nThe branch already has work from an earlier round or run. Inspect it (\`git log master..HEAD\`, \`git diff master...HEAD\`) and continue from it.` : ''}${feedback ? `\n\nFix these problems from the gates or the vetter, then commit:\n${feedback}` : ''}

When done, return the proof command and a commit message for the landed change: a lowercase conventional-commit subject and a body explaining why. The merge queue squashes the branch into one commit with that message. The proof command runs from the repository root of another checkout, so it must not name \`${wt.path}\`.`

const implPrompt = (feedback, prior, exhausted) => `You implement braid review finding ${id}. Work only inside the worktree \`${wt.path}\` (branch \`${wt.branch}\`): use absolute paths under it and run every command from it.

Read the finding, its pitch, and the user's decision with \`${F} show ${id}\` (run that from \`${wt.path}\`). If the decision has answers, they are the user's replies to earlier asks and part of the decision.

The user chose this solution: ${decision}

${exhausted ? `The gates or the vetter still reject the branch after ${MAX_REPAIRS} repair rounds:
${exhausted}

Do not change the branch. If a choice by the user would resolve this, ask. Otherwise return status \`blocked\` with the reason. Return empty strings for the proof command, subject, and body.

${ASK_RULES}` : workRules(feedback, prior)}`

// Gates and the vetter judge a detached snapshot of the candidate commit that
// no implementer works in (plan/2026-10-21-1424/D12), so nothing done in the
// implementation worktree can change a verdict.
const gatePrompt = (impl, snap) => `You are the gate for braid review finding ${id}. Check candidate commit \`${snap.commit}\` in its snapshot checkout \`${snap.path}\`; run every command from there and change nothing. Never look at or run anything in \`${wt.path}\`.

1. Confirm \`git rev-parse HEAD\` is \`${snap.commit}\` and \`git status --porcelain --untracked-files=no\` is empty.
2. Run each acceptance recipe: ${acceptance.join(', ')}. Long ones (VM tests) may wait on a host-wide gate. ${RUN_RULE}
${regression
    ? `3. Red/green: find the red commit with \`git log master..${snap.commit} --grep='^Proof: red' --format=%H\` (exactly one). Check it out in a temporary detached worktree (\`git worktree add --detach <path next to ${snap.path}> <sha>\`), run \`${impl.proof_command}\` there, and confirm it FAILS for the reason the finding describes (an assertion, not a build error). Remove the temporary worktree. Then run it in \`${snap.path}\` and confirm it passes.`
    : plan.proof === 'benchmark'
      ? `3. Benchmark: run \`${impl.proof_command}\` on master (in a temporary detached worktree) and in \`${snap.path}\`, and report both numbers. Pass only if the candidate is better by the claim in the finding's Proof section (\`${F} show ${id}\`).`
      : `3. Preservation: run \`${impl.proof_command}\` and confirm it passes.`}
4. Confirm HEAD is still \`${snap.commit}\` and tracked files are unchanged.

Pass only if every step holds. Return tree \`${snap.tree}\`, the verdict, and a report with the failing output.`

// The vetter approves what lands: the candidate's tree and the squash message,
// not the branch's intermediate commits (plan/2026-10-21-1424/D8).
const vetPrompt = (snap, message) => `You vet braid review finding ${id} before it lands on master unattended. You are independent: you have not seen the implementer's reasoning, and you change nothing.

Read the finding, its pitch, and the user's decision with \`${F} show ${id}\`. The user chose: ${decision}

What lands is one commit on master: the tree of candidate \`${snap.commit}\` with the message below. Review the change with \`git -C ${snap.path} diff master...${snap.commit}\` against the decision, \`docs/design/principles.md\`, the ADRs the change touches, and AGENTS.md. Branch commit messages never land; ignore them. Reject if:
- it applies anything other than the chosen solution, or more than it;
- it is wrong, unsafe, or breaks a principle or ADR;
- the proof does not actually prove the claim (a test that would pass with the bug back);
- docs, README, or doc comments are left out of sync.

The proposed landing message:
<<<
${message}
>>>

Return in \`message\` the exact message to land. If the proposed one describes the change accurately and follows the repo's commit rules (lowercase Conventional Commits subject, a body saying why), return it unchanged. If only the message is wrong, rewrite it yourself and return the rewrite; a message problem alone is never a reason to reject.

Return tree \`${snap.tree}\` after confirming \`git -C ${snap.path} rev-parse HEAD^{tree}\` matches it. List each problem with the change concretely; an approval has none.`

let feedback = null
// Every run starts with the implementer; on a resume it inspects the branch
// and continues, so a crash mid-round loses nothing.
let needImplement = true
let prior = plan.action === 'resume'
let repairs = 0
let rebaseRepairs = 0
let impl = null

// Tell the user a finding is waiting on them (plan/2026-10-21-1424/D10). Best
// effort: the drain summary carries the same news if this fails.
async function notify(text) {
  if (opts.notify === false) return
  const message = `braid ${id}: ${text}`.replace(/\s+/g, ' ').slice(0, 190)
  await agent(
    `Load the PushNotification tool with ToolSearch (query "select:PushNotification"), then call it once with status "proactive" and this exact message. Do nothing else.\n\n${message}`,
    { label: `notify:${id}`, phase: 'Implement', model: 'haiku', effort: 'low' },
  )
}

async function block(reason) {
  await store('block', `${F} block ${id} --reason "$(cat <<'EOF'\n${reason}\nEOF\n)"`, 'Implement',
    () => ({ id: 'string', status: 'string' }))
  log(`${id}: blocked: ${reason}`)
  await notify(`blocked: ${reason}`)
  return { id, status: 'blocked', reason }
}

// The implementer recorded an ask; confirm the store holds it before reporting.
async function asked(question) {
  const now = await store('confirm-ask', `${F} launch ${id}`, 'Implement',
    v => v && v.action === 'stop' ? { id: 'string', action: 'string', reason: 'string' } : { id: 'string', action: 'string' })
  if (now.action !== 'stop' || now.reason !== 'status is blocked, not decided') {
    throw new Error(`${id}: the implementer reported an ask, but the store says ${JSON.stringify(now)}`)
  }
  log(`${id}: asking: ${question}`)
  await notify(`asks: ${question}`)
  return { id, status: 'asking', reason: question }
}

// The implementer's verdict on a round: keep going, or stop with an ask or a block.
async function settle(result) {
  if (!result) return { id, status: 'gap', reason: 'implementer failed; relaunch resumes the branch' }
  if (result.status === 'ask') return await asked(result.reason)
  if (result.status === 'blocked') return await block(result.reason)
  return null
}

while (true) {
  if (needImplement) {
    phase('Implement')
    impl = await agent(implPrompt(feedback, prior), { label: `implement:${id}`, phase: 'Implement', effort: 'xhigh', schema: IMPL })
    const stopped = await settle(impl)
    if (stopped) return stopped
    prior = true
  }
  phase('Gate')
  const snap = await store('snapshot', `${F} snapshot ${id}`, 'Gate',
    () => ({ id: 'string', commit: 'string', tree: 'string', path: 'string' }))
  const gate = await agent(gatePrompt(impl, snap), { label: `gate:${id}`, phase: 'Gate', effort: 'medium', schema: GATE })
  if (!gate) return { id, status: 'gap', reason: 'gate failed; relaunch resumes the branch' }
  let problems = gate.passed && gate.tree === snap.tree ? null : `Gate report:\n${gate.report}`
  let landing = null
  if (!problems) {
    phase('Vet')
    const vet = await agent(vetPrompt(snap, `${impl.subject}\n\n${impl.body}`), { label: `vet:${id}`, phase: 'Vet', effort: 'xhigh', schema: VET })
    if (!vet) return { id, status: 'gap', reason: 'vetter failed; relaunch resumes the branch' }
    if (!vet.approved || vet.tree !== snap.tree || !vet.message.trim()) {
      problems = `Vetter:\n${vet.problems.map(p => `- ${p}`).join('\n') || '- the vetter did not approve this tree with a message'}`
    } else {
      landing = { tree: snap.tree, message: vet.message }
    }
  }
  if (problems) {
    if (repairs >= MAX_REPAIRS) {
      // A rejection the run cannot repair may still be a choice for the user.
      phase('Implement')
      const last = await agent(implPrompt(null, true, problems), { label: `implement:${id}`, phase: 'Implement', effort: 'xhigh', schema: IMPL })
      return (await settle(last)) || await block(`repairs exhausted after ${repairs} rounds. ${problems}`)
    }
    repairs++
    log(`${id}: repair round ${repairs}`)
    feedback = problems
    needImplement = true
    continue
  }

  phase('Land')
  let next = null
  for (let waits = 0; next === null; ) {
    // Exactly the tree and message the vetter approved (plan/2026-10-21-1424/I14).
    const res = await store('land', `${F} land ${id} --tree ${landing.tree} --wait 540 --message - <<'EOF'\n${landing.message}\nEOF`, 'Land',
      v => ({ id: 'string', status: 'string', ...LAND[v && v.status] }))
    if (res.status === 'merged') {
      log(`${id}: merged as ${res.commit || 'an earlier run'}`)
      return { id, status: 'merged', commit: res.commit || null }
    }
    if (res.status === 'wait') {
      if (++waits >= MAX_LAND_WAITS) return { id, status: 'waiting', reason: `${res.reason}; relaunch to retry` }
      log(`${id}: ${res.reason}; waiting`)
    } else if (res.status === 'regate') {
      log(`${id}: master moved; re-gating tree ${res.tree}`)
      next = 'regate'
    } else if (res.status === 'conflict') {
      if (rebaseRepairs >= 1) return await block(`rebase onto master conflicts again in ${res.files.join(', ')}`)
      rebaseRepairs++
      feedback = `Rebase the branch onto master (\`git rebase master\` in ${wt.path}) and resolve the conflicts in ${res.files.join(', ')}, keeping the chosen solution.`
      next = 'repair'
    } else {
      throw new Error(`land: unexpected status ${res.status}`)
    }
  }
  needImplement = next === 'repair'
}
