export const meta = {
  name: 'implement',
  description: 'Implement one decided finding in its own worktree, gate and vet it, and land it on master',
  whenToUse: 'Started by /pick-fix after a decision. args: {id: "YYMMDD-HHMM-NNN"}',
  phases: [
    { title: 'Launch', detail: 'read the decision; stop, resume, or start' },
    { title: 'Implement', detail: 'apply the chosen solution with its proof' },
    { title: 'Gate', detail: 'acceptance recipes and the proof, on one exact tree' },
    { title: 'Vet', detail: 'a fresh reviewer approves or rejects that tree' },
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
// Overridable so a scratch repository can stand in for the real one in tests.
const F = opts.findings || 'python3 scripts/findings/findings.py'
const MAX_REPAIRS = 2
const MAX_LAND_WAITS = 12

const RAN = {
  type: 'object',
  properties: { stdout: { type: 'string' }, exit_code: { type: 'integer' } },
  required: ['stdout', 'exit_code'],
}
async function sh(label, command, phaseName) {
  const r = await agent(
    `Run this exact command once with the Bash tool, with a 600000 ms timeout, and return its stdout verbatim and its exit code. Do nothing else.\n\n${command}`,
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
async function store(label, command, phaseName, shapeOf) {
  const r = await sh(label, command, phaseName)
  if (r.exit_code !== 0) throw new Error(`${label}: ${r.stdout}`)
  let value
  try { value = JSON.parse(r.stdout) } catch (e) { throw new Error(`${label}: store result is not JSON: ${r.stdout}`) }
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
    status: { enum: ['done', 'blocked'] },
    reason: { type: 'string', description: 'why blocked, or a one-line summary of the change' },
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
  },
  required: ['tree', 'approved', 'problems'],
}

const implPrompt = (feedback, prior) => `You implement braid review finding ${id}. Work only inside the worktree \`${wt.path}\` (branch \`${wt.branch}\`): use absolute paths under it and run every command from it.

Read the finding, its pitch, and the user's decision with \`${F} show ${id}\` (run that from \`${wt.path}\`).

The user chose this solution: ${decision}

Rules:
- Apply exactly this solution. If it proves wrong or impossible, do not swap in another one: return status \`blocked\` with the reason, and leave the branch as it is.
- Follow AGENTS.md: principles, ADRs, doc comments, docs in sync, test preambles.
${regression
    ? `- Proof (regression): the finding's Proof section says what the test must show.\n  First commit only the test, and confirm it fails for the right reason. Give that commit a \`Proof: red\` trailer. Then commit the fix and confirm the test passes. Keep exactly one \`Proof: red\` commit on the branch.`
    : `- Proof (${plan.proof}): the finding's Proof section says what must hold.`}
- Make every listed acceptance recipe pass: ${acceptance.join(', ')}. VM recipes take a host-wide gate and may wait; run long commands in the background and wait for them.
- Commit all work on the branch (stage files by name; never --amend, never --no-verify, never push, never touch master). Leave the worktree clean.
${prior ? `\nThe branch already has work from an earlier round or run. Inspect it (\`git log master..HEAD\`, \`git diff master...HEAD\`) and continue from it.` : ''}${feedback ? `\n\nFix these problems from the gates or the vetter, then commit:\n${feedback}` : ''}

Return the proof command and a commit message for the landed change: a lowercase conventional-commit subject and a body explaining why. The merge queue squashes the branch into one commit with that message.`

const gatePrompt = impl => `You are the gate for braid review finding ${id}. Check the branch in \`${wt.path}\`; change nothing.

1. Confirm \`git status --porcelain\` is empty. Record the tree: \`git rev-parse HEAD^{tree}\`.
2. Run each acceptance recipe from \`${wt.path}\`: ${acceptance.join(', ')}. Long ones (VM tests) may wait on a host-wide gate; run them in the background and wait for them to exit.
${regression
    ? `3. Red/green: find the red commit with \`git log master..HEAD --grep='^Proof: red' --format=%H\` (exactly one). Check it out in a temporary detached worktree (\`git worktree add --detach <path next to ${wt.path}> <sha>\`), run \`${impl.proof_command}\` there, and confirm it FAILS for the reason the finding describes (an assertion, not a build error). Remove the temporary worktree. Then run it in \`${wt.path}\` and confirm it passes.`
    : plan.proof === 'benchmark'
      ? `3. Benchmark: run \`${impl.proof_command}\` on master (in a temporary detached worktree) and on the branch, and report both numbers. Pass only if the branch is better by the claim in the finding's Proof section (\`${F} show ${id}\`).`
      : `3. Preservation: run \`${impl.proof_command}\` and confirm it passes.`}
4. Confirm the tree is still the one you recorded and the worktree is clean.

Pass only if every step holds. Return the tree, the verdict, and a report with the failing output.`

const vetPrompt = gate => `You vet braid review finding ${id} before it lands on master unattended. You are independent: you have not seen the implementer's reasoning, and you change nothing.

Read the finding, its pitch, and the user's decision with \`${F} show ${id}\`. The user chose: ${decision}

Review \`git -C ${wt.path} diff master...HEAD\` and \`git -C ${wt.path} log master..HEAD\` against the decision, \`docs/design/principles.md\`, the ADRs the change touches, and AGENTS.md. Reject if:
- it applies anything other than the chosen solution, or more than it;
- it is wrong, unsafe, or breaks a principle or ADR;
- the proof does not actually prove the claim (a test that would pass with the bug back);
- docs, README, or doc comments are left out of sync.

The gate passed tree \`${gate.tree}\`. Confirm \`git -C ${wt.path} rev-parse HEAD^{tree}\` matches and return that tree. List each problem concretely; an approval has none.`

let feedback = null
// Every run starts with the implementer; on a resume it inspects the branch
// and continues, so a crash mid-round loses nothing.
let needImplement = true
let prior = plan.action === 'resume'
let repairs = 0
let rebaseRepairs = 0
let impl = null

async function block(reason) {
  await store('block', `${F} block ${id} --reason "$(cat <<'EOF'\n${reason}\nEOF\n)"`, 'Implement',
    () => ({ id: 'string', status: 'string' }))
  log(`${id}: blocked: ${reason}`)
  return { id, status: 'blocked', reason }
}

while (true) {
  if (needImplement) {
    phase('Implement')
    impl = await agent(implPrompt(feedback, prior), { label: `implement:${id}`, phase: 'Implement', effort: 'xhigh', schema: IMPL })
    if (!impl) return { id, status: 'gap', reason: 'implementer failed; relaunch resumes the branch' }
    if (impl.status === 'blocked') return await block(impl.reason)
    prior = true
  }
  phase('Gate')
  const gate = await agent(gatePrompt(impl), { label: `gate:${id}`, phase: 'Gate', effort: 'medium', schema: GATE })
  if (!gate) return { id, status: 'gap', reason: 'gate failed; relaunch resumes the branch' }
  let problems = gate.passed ? null : `Gate report:\n${gate.report}`
  if (!problems) {
    phase('Vet')
    const vet = await agent(vetPrompt(gate), { label: `vet:${id}`, phase: 'Vet', effort: 'xhigh', schema: VET })
    if (!vet) return { id, status: 'gap', reason: 'vetter failed; relaunch resumes the branch' }
    if (!vet.approved || vet.tree !== gate.tree) {
      problems = `Vetter:\n${vet.problems.map(p => `- ${p}`).join('\n') || '- tree changed during review'}`
    }
  }
  if (problems) {
    if (repairs >= MAX_REPAIRS) return await block(`repairs exhausted after ${repairs} rounds. ${problems}`)
    repairs++
    log(`${id}: repair round ${repairs}`)
    feedback = problems
    needImplement = true
    continue
  }

  phase('Land')
  let next = null
  for (let waits = 0; next === null; ) {
    const message = `${impl.subject}\n\n${impl.body}`
    const res = await store('land', `${F} land ${id} --tree ${gate.tree} --wait 540 --message - <<'EOF'\n${message}\nEOF`, 'Land',
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
