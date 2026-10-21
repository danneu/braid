#!/usr/bin/env python3
"""Script-owned store for review-pipeline findings.

Every write to `findings/` goes through this CLI (plan/2026-10-21-1047/D2):
ingesting reviewer output, applying verdicts, merges and pitches, recording
decisions, and choosing what an implement run does. The deterministic
pipeline logic lives here so the workflows only route agent output.

Implementation progress is never stored. "implementing" and "merged" are
derived from git on every read (plan/2026-10-21-1047/D4), so a crash between
a merge and a status write cannot leave the two disagreeing.

Payloads are JSON, read from `--input FILE` or stdin (`--input -`). Results
are JSON on stdout. A refused request prints `error: ...` and exits 1.
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from datetime import datetime
from pathlib import Path

import yaml

MAIN_BRANCH = "master"
TRAILER = "Finding"

RUN_ID = re.compile(r"^\d{6}-\d{4}$")
FINDING_ID = re.compile(r"^(\d{6}-\d{4})-(\d{3})$")
CANDIDATE_KEY = re.compile(r"^[a-z0-9][a-z0-9-]*$")

# Proof kinds each category accepts (plan/2026-10-21-1047/D3). A pivot may
# change behavior, so it may carry a regression proof instead.
PROOF_KINDS = {
    "correctness": {"regression"},
    "test-gap": {"regression"},
    "performance": {"benchmark"},
    "simplification": {"preservation"},
    "docs": {"preservation"},
    "pivot": {"regression", "preservation"},
}
SEVERITY = {"high": 3, "medium": 2, "low": 1}
CONFIDENCE = {"high": 1.0, "medium": 0.6, "low": 0.3}
EFFORT = {"small": 1, "medium": 2, "large": 4}

# The stored state machine (plan/2026-10-21-1047/D4). "implementing" and
# "merged" are absent on purpose: git holds them.
TRANSITIONS = {
    "open": {"verified", "refuted"},
    "verified": {"pitched"},
    "pitched": {"decided", "deferred", "dropped"},
    "deferred": {"decided", "dropped"},
    "decided": {"blocked"},
    "blocked": {"decided"},
    "refuted": set(),
    "dropped": set(),
}
CLOSED = {"refuted", "dropped"}

FINDING_KEYS = {
    "slice",
    "category",
    "severity",
    "confidence",
    "effort",
    "title",
    "claim",
    "evidence",
    "locations",
    "proof",
    "acceptance",
}
PITCH_KEYS = {"id", "issue", "candidates", "ideal", "rejected", "recommendation"}
CANDIDATE_KEYS = {"key", "title", "summary", "tradeoffs", "worst_case"}


class StoreError(Exception):
    """A request the store refuses; the CLI prints it and exits 1."""


def _require(cond: bool, msg: str) -> None:
    if not cond:
        raise StoreError(msg)


def _obj(raw, keys: set[str], ctx: str, optional: set[str] = frozenset()) -> dict:
    _require(isinstance(raw, dict), f"{ctx}: must be an object")
    missing = keys - optional - set(raw)
    extra = set(raw) - keys
    _require(not missing, f"{ctx}: missing keys {sorted(missing)}")
    _require(not extra, f"{ctx}: unknown keys {sorted(extra)}")
    return raw


def _text(obj: dict, key: str, ctx: str, one_line: bool = False) -> str:
    v = obj.get(key)
    _require(isinstance(v, str) and v.strip() != "", f"{ctx}: '{key}' must be a non-empty string")
    _require(not (one_line and "\n" in v), f"{ctx}: '{key}' must be one line")
    return v


def _enum(obj: dict, key: str, allowed, ctx: str) -> str:
    v = obj.get(key)
    _require(v in allowed, f"{ctx}: '{key}' must be one of {sorted(allowed)}, got {v!r}")
    return v


def _texts(obj: dict, key: str, ctx: str) -> list[str]:
    v = obj.get(key)
    _require(
        isinstance(v, list) and v and all(isinstance(x, str) and x.strip() for x in v),
        f"{ctx}: '{key}' must be a non-empty list of strings",
    )
    return list(v)


def validate_finding(raw, ctx: str) -> dict:
    """Checks one reviewer finding and returns its stored fields."""
    _obj(raw, FINDING_KEYS, ctx)
    category = _enum(raw, "category", PROOF_KINDS, ctx)
    proof = _obj(raw["proof"], {"kind", "check"}, f"{ctx}: proof")
    return {
        "slice": _text(raw, "slice", ctx, one_line=True),
        "category": category,
        "severity": _enum(raw, "severity", SEVERITY, ctx),
        "confidence": _enum(raw, "confidence", CONFIDENCE, ctx),
        "effort": _enum(raw, "effort", EFFORT, ctx),
        "title": _text(raw, "title", ctx, one_line=True),
        "claim": _text(raw, "claim", ctx),
        "evidence": _text(raw, "evidence", ctx),
        "locations": _texts(raw, "locations", ctx),
        "proof": {
            "kind": _enum(proof, "kind", PROOF_KINDS[category], f"{ctx}: proof for {category}"),
            "check": _text(proof, "check", f"{ctx}: proof"),
        },
        "acceptance": _texts(raw, "acceptance", ctx),
    }


def validate_pitch(raw, ctx: str) -> dict:
    """Checks a pitch against plan/2026-10-21-1047/I6: an ideal, a rejection, a recommendation."""
    _obj(raw, PITCH_KEYS, ctx)
    cands = raw["candidates"]
    _require(isinstance(cands, list) and cands, f"{ctx}: 'candidates' must be a non-empty list")
    keys = []
    for i, c in enumerate(cands):
        cctx = f"{ctx}: candidate {i}"
        _obj(c, CANDIDATE_KEYS, cctx)
        key = _text(c, "key", cctx, one_line=True)
        _require(CANDIDATE_KEY.match(key) is not None, f"{cctx}: key {key!r} must be kebab-case")
        _require(key not in keys, f"{cctx}: duplicate key {key!r}")
        keys.append(key)
        for field in CANDIDATE_KEYS - {"key"}:
            _text(c, field, cctx)
    _require(raw["ideal"] in keys, f"{ctx}: 'ideal' must name a candidate key")
    rejected = raw["rejected"]
    _require(isinstance(rejected, list) and rejected, f"{ctx}: 'rejected' must be a non-empty list")
    for i, r in enumerate(rejected):
        rctx = f"{ctx}: rejected {i}"
        _obj(r, {"title", "reason"}, rctx)
        _text(r, "title", rctx)
        _text(r, "reason", rctx)
    rec = _obj(raw["recommendation"], {"key", "justification"}, f"{ctx}: recommendation")
    _require(rec["key"] in keys, f"{ctx}: recommendation must name a candidate key")
    _text(rec, "justification", f"{ctx}: recommendation")
    return {k: raw[k] for k in ("issue", "candidates", "ideal", "rejected", "recommendation")}


def score(f: dict) -> float:
    """Rank score: higher severity and confidence raise it, more effort lowers it."""
    return SEVERITY[f["severity"]] * CONFIDENCE[f["confidence"]] / EFFORT[f["effort"]]


def branch_name(fid: str, decision: int) -> str:
    """One branch per decision, so no run resumes work built for an earlier choice."""
    return f"review/{fid}/{decision}"


class Git:
    """Reads the two facts git owns: decision branches and `Finding:` trailers on master."""

    def __init__(self, repo: Path):
        self.repo = repo
        self._merged: set[str] | None = None

    def _run(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(["git", "-C", str(self.repo), *args], capture_output=True, text=True)

    def branch_exists(self, name: str) -> bool:
        return self._run("show-ref", "--verify", "--quiet", f"refs/heads/{name}").returncode == 0

    def merged_ids(self) -> set[str]:
        if self._merged is None:
            _require(
                self.branch_exists(MAIN_BRANCH),
                f"{self.repo}: no '{MAIN_BRANCH}' branch to derive merged findings from",
            )
            out = self._run(
                "log", MAIN_BRANCH, f"--format=%(trailers:key={TRAILER},valueonly,separator=%x0A)"
            )
            _require(out.returncode == 0, f"git log failed: {out.stderr.strip()}")
            self._merged = {line.strip() for line in out.stdout.splitlines() if line.strip()}
        return self._merged


def _split_frontmatter(text: str, path: Path) -> dict:
    _require(text.startswith("---\n"), f"{path}: missing frontmatter")
    end = text.find("\n---\n", 4)
    _require(end != -1, f"{path}: unterminated frontmatter")
    data = yaml.safe_load(text[4:end])
    _require(isinstance(data, dict), f"{path}: frontmatter is not a mapping")
    return data


def _atomic_write(path: Path, text: str) -> None:
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=".tmp-")
    with os.fdopen(fd, "w") as fh:
        fh.write(text)
    os.replace(tmp, path)


def render(f: dict) -> str:
    """Frontmatter holds the record; the body is a read-only view of it for people."""
    out = ["---", yaml.safe_dump(f, sort_keys=False, width=1000).rstrip(), "---", ""]
    out += [
        "<!-- Generated by scripts/findings/findings.py. Edit through the script, not by hand. -->",
        "",
        f"# {f['id']}: {f['title']}",
        "",
        f"`{f['category']}` | severity {f['severity']} | confidence {f['confidence']}"
        f" | effort {f['effort']} | slice `{f['slice']}`",
        "",
        "## Claim",
        "",
        f["claim"],
        "",
        "## Evidence",
        "",
        f["evidence"],
        "",
        "## Locations",
        "",
        *[f"- `{loc}`" for loc in f["locations"]],
        "",
        f"## Proof ({f['proof']['kind']})",
        "",
        f["proof"]["check"],
        "",
        "## Acceptance",
        "",
        *[f"- `just {r}`" for r in f["acceptance"]],
    ]
    if f.get("verification"):
        out += ["", f"## Verification ({f['verification']['verdict']})", "", f["verification"]["reason"]]
    pitch = f.get("pitch")
    if pitch:
        out += ["", "## Pitch", "", pitch["issue"], "", "### Candidates (best first)", ""]
        for i, c in enumerate(pitch["candidates"], 1):
            tags = [t for t, on in (("ideal", c["key"] == pitch["ideal"]),
                                    ("recommended", c["key"] == pitch["recommendation"]["key"])) if on]
            suffix = f" ({', '.join(tags)})" if tags else ""
            out += [
                f"{i}. **{c['title']}** `{c['key']}`{suffix}",
                f"   - {c['summary']}",
                f"   - Trade-offs: {c['tradeoffs']}",
                f"   - Worst case: {c['worst_case']}",
            ]
        out += ["", "### Rejected", ""]
        out += [f"- **{r['title']}**: {r['reason']}" for r in pitch["rejected"]]
        rec = pitch["recommendation"]
        out += ["", f"### Recommendation: `{rec['key']}`", "", rec["justification"]]
    for d in f.get("decisions", []):
        out += ["", f"## Decision {d['number']}", ""]
        out.append(f"Candidate: `{d['candidate']}`" if d["candidate"] else "Candidate: none (direction in note)")
        if d.get("note"):
            out += ["", d["note"]]
        if d.get("blocked_reason"):
            out += ["", f"Blocked: {d['blocked_reason']}"]
    if f.get("note"):
        out += ["", "## Note", "", f["note"]]
    return "\n".join(out) + "\n"


class Store:
    """The `findings/` tree: one file per finding under `<run-id>/`, plus a generated index."""

    def __init__(self, root: Path, repo: Path):
        self.root = root
        self.git = Git(repo)

    def run_dir(self, run_id: str) -> Path:
        _require(RUN_ID.match(run_id) is not None, f"bad run id {run_id!r}; expected YYMMDD-HHMM")
        return self.root / run_id

    def path(self, fid: str) -> Path:
        m = FINDING_ID.match(fid)
        _require(m is not None, f"bad finding id {fid!r}; expected YYMMDD-HHMM-NNN")
        return self.root / m.group(1) / f"{fid}.md"

    def load(self, fid: str) -> dict:
        path = self.path(fid)
        _require(path.is_file(), f"unknown finding {fid}")
        return _split_frontmatter(path.read_text(), path)

    def save(self, f: dict) -> None:
        _atomic_write(self.path(f["id"]), render(f))

    def findings(self, run_id: str) -> list[dict]:
        d = self.run_dir(run_id)
        _require(d.is_dir(), f"unknown run {run_id}")
        return [self.load(p.stem) for p in sorted(d.glob("*.md")) if FINDING_ID.match(p.stem)]

    @contextlib.contextmanager
    def locked(self):
        """Serializes read-modify-write across concurrent workflows and skills."""
        self.root.mkdir(parents=True, exist_ok=True)
        with open(self.root / ".lock", "w") as fh:
            fcntl.flock(fh, fcntl.LOCK_EX)
            yield

    def state(self, f: dict) -> str:
        """The finding's state with git's facts applied (plan/2026-10-21-1047/D4)."""
        if f["id"] in self.git.merged_ids():
            return "merged"
        if f["status"] == "decided" and self.git.branch_exists(branch_name(f["id"], len(f["decisions"]))):
            return "implementing"
        return f["status"]

    def summary(self, f: dict) -> dict:
        s = {k: f[k] for k in ("id", "title", "category", "severity", "confidence", "effort", "slice")}
        s["state"] = self.state(f)
        s["score"] = round(score(f), 3)
        return s

    def write_index(self, run_id: str) -> Path:
        rows = sorted(self.findings(run_id), key=lambda f: (-score(f), f["id"]))
        live = [f for f in rows if f["status"] not in CLOSED]
        closed = [f for f in rows if f["status"] in CLOSED]
        head = "| Rank | ID | State | Severity | Confidence | Effort | Category | Slice | Title |"
        sep = "|---|---|---|---|---|---|---|---|---|"
        out = [
            f"# Findings {run_id}",
            "",
            "<!-- Generated by scripts/findings/findings.py index. Do not edit. -->",
            "",
            head,
            sep,
        ]
        for i, f in enumerate(live, 1):
            out.append(
                f"| {i} | [{f['id']}](./{f['id']}.md) | {self.state(f)} | {f['severity']} | "
                f"{f['confidence']} | {f['effort']} | {f['category']} | {f['slice']} | {f['title']} |"
            )
        if closed:
            out += ["", "## Closed", "", "| ID | State | Slice | Title |", "|---|---|---|---|"]
            out += [
                f"| [{f['id']}](./{f['id']}.md) | {f['status']} | {f['slice']} | {f['title']} |" for f in closed
            ]
        path = self.run_dir(run_id) / "index.md"
        _atomic_write(path, "\n".join(out) + "\n")
        return path


def _transition(f: dict, to: str) -> None:
    frm = f["status"]
    _require(to in TRANSITIONS[frm], f"{f['id']}: cannot move from {frm} to {to}")
    f["status"] = to


def _run_of(fid: str) -> str:
    return FINDING_ID.match(fid).group(1)


def _list(payload, ctx: str) -> list:
    _require(isinstance(payload, list), f"{ctx}: payload must be a list")
    return payload


def _save_all(store: Store, changed: list[dict], removed: list[str] = ()) -> None:
    """Writes only after every item validated, so a bad payload changes nothing."""
    for f in changed:
        store.save(f)
    for fid in removed:
        store.path(fid).unlink()
    for run_id in sorted({_run_of(f["id"]) for f in changed} | {_run_of(fid) for fid in removed}):
        store.write_index(run_id)


def cmd_new_run(store: Store, run_id: str | None) -> dict:
    run_id = run_id or datetime.now().strftime("%y%m%d-%H%M")
    d = store.run_dir(run_id)
    _require(not d.exists(), f"run {run_id} already exists")
    d.mkdir(parents=True)
    store.write_index(run_id)
    return {"run": run_id}


def cmd_ingest(store: Store, run_id: str, payload) -> dict:
    items = [validate_finding(raw, f"finding {i}") for i, raw in enumerate(_list(payload, "ingest"))]
    existing = store.findings(run_id)
    start = max((int(FINDING_ID.match(f["id"]).group(2)) for f in existing), default=0) + 1
    _require(start + len(items) - 1 <= 999, f"run {run_id} would exceed 999 findings")
    created = []
    for n, item in enumerate(items, start):
        f = {"id": f"{run_id}-{n:03d}", "status": "open", **item, "depends_on": [], "duplicates": [],
             "decisions": []}
        created.append(f)
    _save_all(store, created)
    return {"ids": [f["id"] for f in created]}


def cmd_verify(store: Store, payload) -> dict:
    changed = []
    for i, raw in enumerate(_list(payload, "verify")):
        ctx = f"verdict {i}"
        _obj(raw, {"id", "verdict", "reason"}, ctx)
        f = store.load(raw["id"])
        _transition(f, _enum(raw, "verdict", {"verified", "refuted"}, ctx))
        f["verification"] = {"verdict": raw["verdict"], "reason": _text(raw, "reason", ctx)}
        changed.append(f)
    _require(len({f["id"] for f in changed}) == len(changed), "verify: duplicate ids")
    _save_all(store, changed)
    return {"updated": [f["id"] for f in changed]}


def cmd_merge(store: Store, payload) -> dict:
    """Collapses duplicate findings into one keeper and records dependencies."""
    _obj(payload, {"groups", "depends_on"}, "merge", optional={"groups", "depends_on"})
    groups = payload.get("groups", [])
    _require(isinstance(groups, list), "merge: 'groups' must be a list")
    loaded: dict[str, dict] = {}

    def get(fid: str) -> dict:
        if fid not in loaded:
            loaded[fid] = store.load(fid)
        return loaded[fid]

    seen: set[str] = set()
    removed: list[str] = []
    for i, g in enumerate(groups):
        ctx = f"merge group {i}"
        _obj(g, {"keep", "absorb"}, ctx)
        ids = [g["keep"], *_texts(g, "absorb", ctx)]
        _require(not (set(ids) & seen) and len(set(ids)) == len(ids), f"{ctx}: an id appears twice")
        seen |= set(ids)
        keep = get(g["keep"])
        for fid in ids:
            _require(get(fid)["status"] == "verified", f"{ctx}: {fid} is {get(fid)['status']}, not verified")
        for fid in g["absorb"]:
            dup = get(fid)
            keep["locations"] += [loc for loc in dup["locations"] if loc not in keep["locations"]]
            keep["duplicates"] += [fid, *dup["duplicates"]]
            removed.append(fid)
    deps = payload.get("depends_on", {})
    _require(isinstance(deps, dict), "merge: 'depends_on' must be an object")
    for fid, targets in deps.items():
        ctx = f"depends_on {fid}"
        targets = _texts(deps, fid, ctx)
        _require(fid not in removed and not set(targets) & set(removed), f"{ctx}: names an absorbed finding")
        _require(fid not in targets, f"{ctx}: a finding cannot depend on itself")
        for t in targets:
            store.load(t)
        get(fid)["depends_on"] = targets
    _save_all(store, [f for fid, f in loaded.items() if fid not in removed], removed)
    return {"kept": [g["keep"] for g in groups], "removed": removed}


def cmd_pitch(store: Store, payload) -> dict:
    changed = []
    for i, raw in enumerate(_list(payload, "pitch")):
        ctx = f"pitch {i}"
        _require(isinstance(raw, dict) and "id" in raw, f"{ctx}: missing 'id'")
        pitch = validate_pitch(raw, ctx)
        f = store.load(raw["id"])
        _transition(f, "pitched")
        f["pitch"] = pitch
        changed.append(f)
    _require(len({f["id"] for f in changed}) == len(changed), "pitch: duplicate ids")
    _save_all(store, changed)
    return {"updated": [f["id"] for f in changed]}


def cmd_decide(store: Store, fid: str, candidate: str | None, note: str | None) -> dict:
    """Records the user's choice; each decision gets the next number and its own branch."""
    f = store.load(fid)
    _require(store.state(f) != "merged", f"{fid}: already merged")
    _transition(f, "decided")
    _require(candidate or note, f"{fid}: a decision needs a candidate, a note, or both")
    if candidate:
        keys = [c["key"] for c in f["pitch"]["candidates"]]
        _require(candidate in keys, f"{fid}: no candidate {candidate!r}; have {keys}")
    decision = {"number": len(f["decisions"]) + 1, "candidate": candidate, "note": note}
    f["decisions"].append(decision)
    _save_all(store, [f])
    return {"id": fid, "decision": decision, "branch": branch_name(fid, decision["number"])}


def cmd_close(store: Store, fid: str, to: str, note: str | None) -> dict:
    f = store.load(fid)
    _transition(f, to)
    if note:
        f["note"] = note
    _save_all(store, [f])
    return {"id": fid, "status": to}


def cmd_block(store: Store, fid: str, reason: str) -> dict:
    f = store.load(fid)
    _require(store.state(f) != "merged", f"{fid}: already merged")
    _require(reason.strip() != "", f"{fid}: a block needs a reason")
    _transition(f, "blocked")
    f["decisions"][-1]["blocked_reason"] = reason
    _save_all(store, [f])
    return {"id": fid, "status": "blocked"}


def cmd_launch(store: Store, fid: str) -> dict:
    """What an implement run may do, decided before any side effect (plan/2026-10-21-1047/D12)."""
    f = store.load(fid)
    if store.state(f) == "merged":
        return {"id": fid, "action": "stop", "reason": "merged"}
    if f["status"] != "decided":
        return {"id": fid, "action": "stop", "reason": f"status is {f['status']}, not decided"}
    decision = f["decisions"][-1]
    branch = branch_name(fid, decision["number"])
    chosen = next((c for c in f["pitch"]["candidates"] if c["key"] == decision["candidate"]), None)
    return {
        "id": fid,
        "action": "resume" if store.git.branch_exists(branch) else "start",
        "branch": branch,
        "trailer": f"{TRAILER}: {fid}",
        "decision": decision,
        "candidate": chosen,
        "finding": {k: f[k] for k in ("title", "category", "claim", "locations", "proof", "acceptance")},
    }


def _git(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    out = subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, text=True)
    _require(not check or out.returncode == 0, f"git {' '.join(args)} failed: {out.stderr.strip()}")
    return out


def _worktree_of(repo: Path, branch: str) -> Path | None:
    path = None
    for line in _git(repo, "worktree", "list", "--porcelain").stdout.splitlines():
        if line.startswith("worktree "):
            path = Path(line[len("worktree "):])
        elif line == f"branch refs/heads/{branch}":
            return path
    return None


def _authorized(store: Store, fid: str) -> dict:
    plan = cmd_launch(store, fid)
    _require(plan["action"] != "stop", f"{fid}: {plan.get('reason')}")
    return plan


def cmd_worktree(store: Store, repo: Path, fid: str) -> dict:
    """The worktree for the current decision's branch: reused if present, else cut from master."""
    plan = _authorized(store, fid)
    branch = plan["branch"]
    existing = _worktree_of(repo, branch)
    if existing:
        return {"id": fid, "branch": branch, "path": str(existing.resolve()), "created": False}
    path = repo / ".claude" / "worktrees" / branch.replace("/", "-")
    if plan["action"] == "resume":
        _git(repo, "worktree", "add", str(path), branch)
    else:
        _git(repo, "worktree", "add", "-b", branch, str(path), MAIN_BRANCH)
    return {"id": fid, "branch": branch, "path": str(path.resolve()), "created": True}


def _dirty(path: Path) -> list[str]:
    return _git(path, "status", "--porcelain", "--untracked-files=no").stdout.splitlines()


def cmd_land(store: Store, repo: Path, fid: str, tree: str, message: str, wait: float) -> dict:
    """The merge queue (plan/2026-10-21-1047/D9): lands the approved tree as one commit on master.

    Only the exact tree the gates approved lands. A rebase onto a moved
    master changes the tree, so it returns `regate` and the gates run again.
    master moves only by fast-forward while the main checkout is clean.

    The merged check, the rebase, and the commit share one hold of the queue
    lock (plan/2026-10-21-1424/D11), so of two runs landing one finding, the
    second sees the first's trailer and stops. The queue lock is taken before
    the store lock, never the other way round.
    """
    _require(message.strip() != "", f"{fid}: a landing needs a commit message")
    store.root.mkdir(parents=True, exist_ok=True)
    with open(store.root / ".queue.lock", "w") as queue:
        fcntl.flock(queue, fcntl.LOCK_EX)
        with store.locked():
            # Nothing before the queue lock may read git's merged set: it is
            # cached per process, and only a read under the lock is current.
            f = store.load(fid)
            if store.state(f) == "merged":
                wt = _worktree_of(repo, branch_name(fid, len(f["decisions"])))
                if wt:
                    _git(repo, "worktree", "remove", "--force", str(wt))
                return {"id": fid, "status": "merged", "already": True}
            plan = _authorized(store, fid)
        wt = _worktree_of(repo, plan["branch"])
        _require(wt is not None, f"{fid}: no worktree for {plan['branch']}")
        _require(not _git(wt, "status", "--porcelain").stdout, f"{fid}: worktree {wt} has uncommitted changes")
        deadline = time.monotonic() + wait
        while _dirty(repo):
            if time.monotonic() >= deadline:
                return {"id": fid, "status": "wait", "reason": f"main checkout {repo} has uncommitted changes"}
            time.sleep(min(15, max(0.1, deadline - time.monotonic())))
        base = _git(repo, "rev-parse", f"refs/heads/{MAIN_BRANCH}").stdout.strip()
        if _git(repo, "merge-base", "--is-ancestor", base, plan["branch"], check=False).returncode != 0:
            if _git(wt, "rebase", base, check=False).returncode != 0:
                conflicts = _git(wt, "diff", "--name-only", "--diff-filter=U").stdout.split()
                _git(wt, "rebase", "--abort")
                return {"id": fid, "status": "conflict", "files": conflicts}
        tip_tree = _git(wt, "rev-parse", "HEAD^{tree}").stdout.strip()
        if tip_tree != tree:
            return {"id": fid, "status": "regate", "tree": tip_tree}
        commit = _git(repo, "commit-tree", tree, "-p", base, "-m",
                      f"{message.rstrip()}\n\n{plan['trailer']}\n").stdout.strip()
        head = _git(repo, "symbolic-ref", "--quiet", "HEAD", check=False).stdout.strip()
        if head == f"refs/heads/{MAIN_BRANCH}":
            if _git(repo, "merge", "--ff-only", "--quiet", commit, check=False).returncode != 0:
                return {"id": fid, "status": "wait", "reason": "fast-forward refused; the main checkout changed"}
        else:
            _git(repo, "update-ref", f"refs/heads/{MAIN_BRANCH}", commit, base)
        # Under the lock, so a concurrent run of this finding finds it merged
        # with nothing left to clean up.
        _git(repo, "worktree", "remove", str(wt))
    return {"id": fid, "status": "merged", "commit": commit}


def _main_checkout() -> Path:
    """The main worktree, so linked worktrees share one store."""
    out = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], capture_output=True, text=True
    )
    _require(out.returncode == 0, "not inside a git repository")
    return Path(out.stdout.strip()).parent


def _read_payload(path: str):
    text = sys.stdin.read() if path == "-" else Path(path).read_text()
    try:
        return json.loads(text)
    except json.JSONDecodeError as e:
        raise StoreError(f"payload is not JSON: {e}") from e


def _parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--store", type=Path, help="store root (default: <main checkout>/findings)")
    p.add_argument("--repo", type=Path, help="git repository (default: the main checkout)")
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("new-run", help="create a run directory and print its id")
    s.add_argument("--id")
    s = sub.add_parser("ingest", help="add reviewer findings to a run as open")
    s.add_argument("run")
    s.add_argument("--input", required=True)
    for name, desc in (("verify", "apply skeptic verdicts"), ("merge", "collapse duplicates, set dependencies"),
                       ("pitch", "attach pitches to verified findings")):
        sub.add_parser(name, help=desc).add_argument("--input", required=True)
    s = sub.add_parser("decide", help="record the user's choice for a pitched or blocked finding")
    s.add_argument("id")
    s.add_argument("--candidate")
    s.add_argument("--note")
    for name in ("defer", "drop"):
        s = sub.add_parser(name)
        s.add_argument("id")
        s.add_argument("--note")
    s = sub.add_parser("block", help="stop a decided finding until the user decides again")
    s.add_argument("id")
    s.add_argument("--reason", required=True)
    sub.add_parser("launch", help="what an implement run may do now").add_argument("id")
    sub.add_parser("show", help="print one finding with its derived state").add_argument("id")
    s = sub.add_parser("list", help="list a run's findings in rank order")
    s.add_argument("run")
    s.add_argument("--state", action="append", help="keep only these derived states (repeatable)")
    sub.add_parser("index", help="regenerate a run's index.md").add_argument("run")
    sub.add_parser("worktree", help="create or reuse the worktree for the current decision").add_argument("id")
    s = sub.add_parser("land", help="merge queue: land the approved tree on master as one commit")
    s.add_argument("id")
    s.add_argument("--tree", required=True, help="the tree the gates and the vetter approved")
    s.add_argument("--message", required=True, help="commit message file, or - for stdin")
    s.add_argument("--wait", type=float, default=0, help="seconds to wait for a clean main checkout")
    return p


def _dispatch(store: Store, repo: Path, args) -> object:
    if args.cmd == "new-run":
        return cmd_new_run(store, args.id)
    if args.cmd == "ingest":
        return cmd_ingest(store, args.run, _read_payload(args.input))
    if args.cmd == "verify":
        return cmd_verify(store, _read_payload(args.input))
    if args.cmd == "merge":
        return cmd_merge(store, _read_payload(args.input))
    if args.cmd == "pitch":
        return cmd_pitch(store, _read_payload(args.input))
    if args.cmd == "decide":
        return cmd_decide(store, args.id, args.candidate, args.note)
    if args.cmd in ("defer", "drop"):
        return cmd_close(store, args.id, {"defer": "deferred", "drop": "dropped"}[args.cmd], args.note)
    if args.cmd == "block":
        return cmd_block(store, args.id, args.reason)
    if args.cmd == "launch":
        return cmd_launch(store, args.id)
    if args.cmd == "worktree":
        return cmd_worktree(store, repo, args.id)
    if args.cmd == "show":
        f = store.load(args.id)
        return {**f, "state": store.state(f), "score": round(score(f), 3)}
    if args.cmd == "list":
        rows = sorted(store.findings(args.run), key=lambda f: (-score(f), f["id"]))
        result = [store.summary(f) for f in rows]
        return [r for r in result if r["state"] in args.state] if args.state else result
    return {"index": str(store.write_index(args.run))}


def main(argv: list[str]) -> int:
    args = _parser().parse_args(argv)
    try:
        repo = args.repo or _main_checkout()
        store = Store(args.store or repo / "findings", repo)
        if args.cmd == "land":
            # cmd_land takes the store lock only to read state, so a long wait
            # for a clean checkout never blocks other store commands. It holds
            # the queue lock throughout.
            text = sys.stdin.read() if args.message == "-" else Path(args.message).read_text()
            result = cmd_land(store, repo, args.id, args.tree, text, args.wait)
        else:
            with store.locked():
                result = _dispatch(store, repo, args)
    except StoreError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
