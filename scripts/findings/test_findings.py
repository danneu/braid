"""Tests for the finding store CLI (plan/2026-10-21-1047/PO1, plan/2026-10-21-1047/PO7).

Run: python3 -m unittest discover -s scripts/findings
"""

from __future__ import annotations

import contextlib
import fcntl
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import findings  # noqa: E402

RUN = "261021-1100"


def finding(**over) -> dict:
    base = {
        "slice": "command:add",
        "category": "correctness",
        "severity": "medium",
        "confidence": "medium",
        "effort": "medium",
        "title": "add skips the LUKS2 check on a reused disk",
        "claim": "The claim.",
        "evidence": "The evidence.",
        "locations": ["cli/src/add.rs#plan_add"],
        "proof": {"kind": "regression", "check": "a unit test that adds a LUKS1 disk"},
        "acceptance": ["test-rust"],
    }
    base.update(over)
    return base


def pitch(fid: str, keys=("ideal-fix", "local-fix")) -> dict:
    return {
        "id": fid,
        "issue": "The issue.",
        "candidates": [
            {"key": k, "title": k, "summary": "s", "tradeoffs": "t", "worst_case": "w"} for k in keys
        ],
        "ideal": keys[0],
        "rejected": [{"title": "do nothing", "reason": "the bug stays"}],
        "recommendation": {"key": keys[0], "justification": "j"},
    }


class StoreTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.repo = Path(tmp.name) / "repo"
        self.store = Path(tmp.name) / "store"
        env = mock.patch.dict(
            os.environ,
            {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
             "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
             "GIT_COMMITTER_EMAIL": "t@t"},
        )
        env.start()
        self.addCleanup(env.stop)
        self.repo.mkdir()
        self.git("init", "-q", "-b", "master")
        self.git("commit", "-q", "--allow-empty", "-m", "root")
        self.cli("new-run", "--id", RUN)

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.repo), *args], check=True, capture_output=True,
                              text=True).stdout

    def run_cli(self, *args: str, payload=None, text: str = "") -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        stdin = io.StringIO(json.dumps(payload) if payload is not None else text)
        with mock.patch.object(sys, "stdin", stdin), contextlib.redirect_stdout(out), \
                contextlib.redirect_stderr(err):
            rc = findings.main(["--store", str(self.store), "--repo", str(self.repo), *args])
        return rc, out.getvalue(), err.getvalue()

    def cli(self, *args: str, payload=None, text: str = ""):
        rc, out, err = self.run_cli(*args, payload=payload, text=text)
        self.assertEqual(rc, 0, err)
        return json.loads(out)

    def refused(self, *args: str, payload=None) -> str:
        rc, _, err = self.run_cli(*args, payload=payload)
        self.assertEqual(rc, 1, f"expected refusal for {args}")
        return err

    def ingest(self, *items: dict) -> list[str]:
        return self.cli("ingest", RUN, "--input", "-", payload=list(items))["ids"]

    def state(self, fid: str) -> str:
        return self.cli("show", fid)["state"]

    def to_pitched(self, *items: dict) -> list[str]:
        ids = self.ingest(*items)
        self.cli("verify", "--input", "-", payload=[{"id": i, "verdict": "verified", "reason": "r"} for i in ids])
        self.cli("pitch", "--input", "-", payload=[pitch(i) for i in ids])
        return ids

    def land(self, fid: str, branch: str = "master") -> None:
        self.git("checkout", "-q", "-B", branch)
        self.git("commit", "-q", "--allow-empty", "-m", f"fix\n\nFinding: {fid}")
        self.git("checkout", "-q", "master")


class TransitionTest(StoreTest):
    def test_walks_every_legal_transition(self):
        a, b, c, d = self.ingest(finding(), finding(), finding(), finding())
        self.cli("verify", "--input", "-", payload=[
            {"id": a, "verdict": "verified", "reason": "r"},
            {"id": b, "verdict": "refuted", "reason": "r"},
            {"id": c, "verdict": "verified", "reason": "r"},
            {"id": d, "verdict": "verified", "reason": "r"},
        ])
        self.cli("pitch", "--input", "-", payload=[pitch(a), pitch(c), pitch(d)])
        self.cli("decide", a, "--candidate", "ideal-fix")
        self.cli("block", a, "--reason", "impossible")
        self.cli("decide", a, "--candidate", "local-fix")
        self.cli("defer", c)
        self.cli("decide", c, "--note", "do it differently")
        self.cli("defer", d)
        self.cli("drop", d)
        self.assertEqual([self.state(x) for x in (a, b, c, d)], ["decided", "refuted", "decided", "dropped"])

    def test_refuses_illegal_transitions_and_changes_nothing(self):
        open_, refuted, pitched = self.ingest(finding(), finding(), finding())
        self.cli("verify", "--input", "-", payload=[
            {"id": refuted, "verdict": "refuted", "reason": "r"},
            {"id": pitched, "verdict": "verified", "reason": "r"},
        ])
        self.cli("pitch", "--input", "-", payload=[pitch(pitched)])
        before = {i: self.cli("show", i) for i in (open_, refuted, pitched)}
        cases = [
            ("pitch", "--input", "-", [pitch(open_)]),
            ("pitch", "--input", "-", [pitch(refuted)]),
            ("decide", open_, "--candidate", "ideal-fix", None),
            ("decide", refuted, "--note", "n", None),
            ("block", pitched, "--reason", "r", None),
            ("verify", "--input", "-", [{"id": pitched, "verdict": "verified", "reason": "r"}]),
            # A batch with one legal and one illegal item applies neither.
            ("verify", "--input", "-",
             [{"id": open_, "verdict": "verified", "reason": "r"},
              {"id": refuted, "verdict": "verified", "reason": "r"}]),
        ]
        for *args, payload in cases:
            with self.subTest(args=args):
                self.refused(*args, payload=payload)
        self.assertEqual({i: self.cli("show", i) for i in before}, before)

    def test_derived_states_follow_git(self):
        (fid,) = self.to_pitched(finding())
        self.cli("decide", fid, "--candidate", "ideal-fix")
        self.assertEqual(self.state(fid), "decided")
        self.git("branch", f"review/{fid}/1")
        self.assertEqual(self.state(fid), "implementing")
        self.land(fid, branch="elsewhere")
        self.assertEqual(self.state(fid), "implementing", "a trailer off master is not a merge")
        self.land(fid)
        self.assertEqual(self.state(fid), "merged")
        self.refused("block", fid, "--reason", "r")

    def test_linked_worktrees_share_the_main_checkout_store(self):
        worktree = self.repo.parent / "wt"
        self.git("worktree", "add", "-q", str(worktree))
        cwd = os.getcwd()
        self.addCleanup(os.chdir, cwd)
        os.chdir(worktree)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(findings.main(["new-run", "--id", "261021-1200"]), 0)
        self.assertTrue((self.repo / "findings" / "261021-1200").is_dir())
        self.assertFalse((worktree / "findings").exists())


class PipelineTest(StoreTest):
    def test_merge_collapses_duplicates_into_the_keeper(self):
        keep, dup1, dup2 = self.ingest(
            finding(locations=["a.rs#x"]), finding(locations=["b.rs#y", "a.rs#x"]), finding(locations=["c.rs#z"])
        )
        self.cli("verify", "--input", "-", payload=[{"id": i, "verdict": "verified", "reason": "r"}
                                                    for i in (keep, dup1, dup2)])
        self.cli("merge", "--input", "-", payload={"groups": [{"keep": keep, "absorb": [dup1, dup2]}]})
        self.assertEqual([r["id"] for r in self.cli("list", RUN)], [keep])
        kept = self.cli("show", keep)
        self.assertEqual(kept["locations"], ["a.rs#x", "b.rs#y", "c.rs#z"])
        self.assertEqual(kept["duplicates"], [dup1, dup2])

    def test_merge_refuses_unverified_findings(self):
        keep, open_ = self.ingest(finding(), finding())
        self.cli("verify", "--input", "-", payload=[{"id": keep, "verdict": "verified", "reason": "r"}])
        self.refused("merge", "--input", "-", payload={"groups": [{"keep": keep, "absorb": [open_]}]})
        self.assertEqual(len(self.cli("list", RUN)), 2)

    def test_rank_follows_severity_confidence_and_effort(self):
        # Ingest order is the reverse of the expected rank, so ties (broken by
        # id) cannot make an ignored input look honored.
        conf, effort, base, sev = self.ingest(
            finding(title="lower confidence", confidence="low"),
            finding(title="more effort", effort="large"),
            finding(title="base"),
            finding(title="higher severity", severity="high"),
        )
        order = [r["id"] for r in self.cli("list", RUN)]
        self.assertLess(order.index(sev), order.index(base))
        self.assertLess(order.index(base), order.index(conf))
        self.assertLess(order.index(base), order.index(effort))
        index = (self.store / RUN / "index.md").read_text()
        self.assertLess(index.index(sev), index.index(base))

    def test_launch_selects_the_recorded_decision(self):
        (fid,) = self.to_pitched(finding())
        self.assertEqual(self.cli("launch", fid)["action"], "stop", "a pitched finding is not authorized")
        self.cli("decide", fid, "--candidate", "local-fix", "--note", "keep the old flag")
        first = self.cli("launch", fid)
        self.assertEqual(
            (first["action"], first["branch"], first["decision"], first["candidate"]),
            ("start", f"review/{fid}/1", 1, "local-fix"),
        )
        self.assertEqual(self.cli("show", fid)["decisions"][-1]["note"], "keep the old flag")
        self.git("branch", first["branch"])
        self.assertEqual(self.cli("launch", fid)["action"], "resume")
        self.cli("block", fid, "--reason", "the flag cannot stay")
        self.assertEqual(self.cli("launch", fid)["action"], "stop", "a blocked branch is never resumed")
        self.cli("decide", fid, "--candidate", "ideal-fix")
        second = self.cli("launch", fid)
        self.assertEqual((second["action"], second["branch"], second["candidate"]),
                         ("start", f"review/{fid}/2", "ideal-fix"))
        self.land(fid)
        self.assertEqual(self.cli("launch", fid), {"id": fid, "action": "stop", "reason": "merged"})


class QueueTest(StoreTest):
    """The merge queue (plan/2026-10-21-1047/PO4) and restart safety (plan/2026-10-21-1047/PO8)."""

    def decided(self) -> tuple[str, Path]:
        (fid,) = self.to_pitched(finding())
        self.cli("decide", fid, "--candidate", "ideal-fix")
        return fid, Path(self.cli("worktree", fid)["path"])

    def commit_in(self, wt: Path, name: str, content: str) -> str:
        (wt / name).write_text(content)
        subprocess.run(["git", "-C", str(wt), "add", name], check=True)
        subprocess.run(["git", "-C", str(wt), "commit", "-q", "-m", f"edit {name}"], check=True)
        return subprocess.run(["git", "-C", str(wt), "rev-parse", "HEAD^{tree}"], check=True,
                              capture_output=True, text=True).stdout.strip()

    def land(self, fid: str, tree: str) -> dict:
        return self.cli("land", fid, "--tree", tree, "--message", "-", text="fix: the thing\n\nWhy.\n")

    def master(self) -> str:
        return self.git("rev-parse", "master").strip()

    def test_lands_the_approved_tree_as_one_trailer_commit(self):
        fid, wt = self.decided()
        self.assertEqual(self.state(fid), "implementing")
        self.commit_in(wt, "a.txt", "red\n")
        tree = self.commit_in(wt, "a.txt", "green\n")
        before = self.master()
        self.assertEqual(self.land(fid, tree)["status"], "merged")
        self.assertEqual(self.git("rev-parse", "master^").strip(), before, "one commit on top of master")
        self.assertEqual(self.git("rev-parse", "master^{tree}").strip(), tree)
        self.assertIn(f"Finding: {fid}", self.git("log", "-1", "--format=%B", "master"))
        self.assertEqual((self.repo / "a.txt").read_text(), "green\n", "the main checkout moved with master")
        self.assertFalse(wt.exists())
        self.assertEqual(self.state(fid), "merged")
        # A restart after the fast-forward finds the finding merged and does nothing.
        self.assertEqual(self.land(fid, tree), {"id": fid, "status": "merged", "already": True})
        self.assertEqual(self.cli("launch", fid)["action"], "stop")
        self.assertEqual(self.git("rev-list", "--count", "--grep", f"Finding: {fid}", "master").strip(), "1")

    def test_waits_while_the_main_checkout_is_dirty(self):
        (self.repo / "notes.txt").write_text("v1\n")
        self.git("add", "notes.txt")
        self.git("commit", "-q", "-m", "notes")
        fid, wt = self.decided()
        tree = self.commit_in(wt, "a.txt", "fix\n")
        (self.repo / "notes.txt").write_text("user edit\n")
        before = self.master()
        self.assertEqual(self.land(fid, tree)["status"], "wait")
        self.assertEqual(self.master(), before)
        self.git("checkout", "notes.txt")
        self.assertEqual(self.land(fid, tree)["status"], "merged")

    def test_a_moved_master_sends_the_rebased_tree_back_through_the_gates(self):
        first, wt1 = self.decided()
        second, wt2 = self.decided()
        tree1 = self.commit_in(wt1, "a.txt", "a\n")
        tree2 = self.commit_in(wt2, "b.txt", "b\n")
        self.assertEqual(self.land(first, tree1)["status"], "merged")
        landed = self.master()
        regate = self.land(second, tree2)
        self.assertEqual(regate["status"], "regate")
        self.assertNotEqual(regate["tree"], tree2)
        self.assertEqual(self.master(), landed, "an unapproved tree never lands")
        self.assertEqual(self.land(second, regate["tree"])["status"], "merged")
        self.assertEqual(self.git("rev-parse", "master^").strip(), landed)

    def test_a_conflicting_rebase_is_aborted_and_reported(self):
        first, wt1 = self.decided()
        second, wt2 = self.decided()
        tree1 = self.commit_in(wt1, "x.txt", "one\n")
        tree2 = self.commit_in(wt2, "x.txt", "two\n")
        self.land(first, tree1)
        tip = subprocess.run(["git", "-C", str(wt2), "rev-parse", "HEAD"], capture_output=True, text=True).stdout
        self.assertEqual(self.land(second, tree2), {"id": second, "status": "conflict", "files": ["x.txt"]})
        self.assertEqual(subprocess.run(["git", "-C", str(wt2), "rev-parse", "HEAD"], capture_output=True,
                                        text=True).stdout, tip, "the branch is left as it was")
        self.assertEqual(self.state(second), "implementing")

    def test_a_tree_other_than_the_approved_one_never_lands(self):
        fid, wt = self.decided()
        approved = self.commit_in(wt, "a.txt", "approved\n")
        self.commit_in(wt, "a.txt", "changed after review\n")
        before = self.master()
        self.assertEqual(self.land(fid, approved)["status"], "regate")
        self.assertEqual(self.master(), before)

    def assert_control(self, result: dict, keys: set[str]) -> None:
        """Short one-line values only, so an agent relay cannot abridge them (plan/2026-10-21-1424/I10)."""
        self.assertEqual(set(result), keys, result)

        def short(v) -> bool:
            if isinstance(v, str):
                return "\n" not in v and len(v) <= 200
            if isinstance(v, list):
                return all(short(x) for x in v)
            return v is None or isinstance(v, (bool, int))

        for k, v in result.items():
            self.assertTrue(short(v), f"{k} is not a short control value: {v!r}")

    def test_workflow_facing_commands_return_only_control_fields(self):
        # plan/2026-10-21-1424/PO1: launch, worktree, snapshot, block, and land are what
        # implement.js parses; the claim, pitch, and note never reach it.
        long_note = "keep the old flag\n" * 20
        (pitched,) = self.to_pitched(finding(claim="c" * 500))
        self.assert_control(self.cli("launch", pitched), {"id", "action", "reason"})
        self.cli("decide", pitched, "--candidate", "ideal-fix", "--note", long_note)
        launch = {"id", "action", "branch", "decision", "candidate", "proof", "acceptance"}
        self.assert_control(self.cli("launch", pitched), launch)
        self.assert_control(self.cli("worktree", pitched), {"id", "branch", "path", "created"})
        self.assert_control(self.cli("launch", pitched), launch)
        self.assert_control(self.cli("snapshot", pitched), {"id", "commit", "tree", "path"})
        self.assert_control(self.cli("block", pitched, "--reason", "r"), {"id", "status"})

        first, wt1 = self.decided()
        second, wt2 = self.decided()
        tree1 = self.commit_in(wt1, "x.txt", "one\n")
        tree2 = self.commit_in(wt2, "x.txt", "two\n")
        (self.repo / "dirt.txt").write_text("d\n")
        self.git("add", "dirt.txt")
        self.assert_control(self.land(first, tree1), {"id", "status", "reason"})
        self.git("rm", "-q", "--cached", "dirt.txt")
        (self.repo / "dirt.txt").unlink()
        self.assert_control(self.land(first, tree1), {"id", "status", "commit"})
        self.assert_control(self.land(first, tree1), {"id", "status", "already"})
        self.assert_control(self.land(second, tree2), {"id", "status", "files"})
        third, wt3 = self.decided()
        tree3 = self.commit_in(wt3, "y.txt", "three\n")
        (self.repo / "z.txt").write_text("z\n")
        self.git("add", "z.txt")
        self.git("commit", "-q", "-m", "master moves")
        self.assert_control(self.land(third, tree3), {"id", "status", "tree"})

    def test_the_gate_snapshot_ignores_the_implementation_worktree(self):
        # plan/2026-10-21-1424/PO11: the gates judge a detached snapshot of the
        # candidate commit. Edits in the implementation worktree, committed or
        # not, cannot make a failing candidate pass.
        fid, wt = self.decided()
        self.commit_in(wt, "check.sh", "exit 1\n")
        snap = self.cli("snapshot", fid)
        path = Path(snap["path"])
        self.assertNotEqual(path, wt.resolve())
        self.assertEqual(snap["commit"], self.git("rev-parse", f"review/{fid}/1").strip())

        def check() -> int:
            return subprocess.run(["sh", "check.sh"], cwd=path).returncode

        self.assertEqual(check(), 1)
        (wt / "check.sh").write_text("exit 0\n")
        self.assertEqual(check(), 1, "an uncommitted edit leaks into the snapshot")
        fixed_tree = self.commit_in(wt, "check.sh", "exit 0\n")
        self.assertEqual(check(), 1, "a later commit leaks into the snapshot")
        self.assertEqual(subprocess.run(["git", "-C", str(path), "rev-parse", "HEAD^{tree}"], capture_output=True,
                                        text=True).stdout.strip(), snap["tree"])
        # The next candidate gets its own snapshot; the old one is cleaned up,
        # and landing removes the rest.
        nxt = self.cli("snapshot", fid)
        self.assertEqual(nxt["tree"], fixed_tree)
        self.assertFalse(path.exists())
        self.assertEqual(self.cli("snapshot", fid), nxt, "the same candidate reuses its snapshot")
        self.land(fid, fixed_tree)
        self.assertFalse(Path(nxt["path"]).exists())
        self.refused("snapshot", fid)

    def test_two_concurrent_landings_of_one_finding_land_it_once(self):
        # plan/2026-10-21-1424/PO10: two runs of one finding both hold the
        # approved tree. Each must wait on the queue lock while the other holds
        # it, so the test holds the lock until both processes are queued.
        fid, wt = self.decided()
        tree = self.commit_in(wt, "a.txt", "fix\n")
        cmd = [sys.executable, str(Path(findings.__file__)), "--store", str(self.store), "--repo",
               str(self.repo), "land", fid, "--tree", tree, "--message", "-"]
        self.store.mkdir(parents=True, exist_ok=True)
        with open(self.store / ".queue.lock", "w") as queue:
            fcntl.flock(queue, fcntl.LOCK_EX)
            procs = [subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                      text=True) for _ in range(2)]
            for p in procs:
                p.stdin.write("fix: the thing\n")
                p.stdin.close()
            time.sleep(1)  # let both processes reach the lock
        results = []
        for p in procs:
            out, err = p.communicate(timeout=60)
            self.assertEqual(p.returncode, 0, err)
            results.append(json.loads(out))
        self.assertEqual(sorted(r["status"] for r in results), ["merged", "merged"])
        self.assertEqual(sorted(bool(r.get("already")) for r in results), [False, True])
        self.assertEqual(self.git("rev-list", "--count", "--grep", f"Finding: {fid}", "master").strip(), "1")

    def test_a_blocked_finding_gets_no_worktree_and_cannot_land(self):
        fid, wt = self.decided()
        tree = self.commit_in(wt, "a.txt", "fix\n")
        self.cli("block", fid, "--reason", "the chosen fix is impossible")
        self.refused("worktree", fid)
        self.refused("land", fid, "--tree", tree, "--message", "-")
        self.assertNotIn(fid, self.git("log", "--format=%B", "master"))
        self.cli("decide", fid, "--candidate", "local-fix")
        fresh = self.cli("worktree", fid)
        self.assertEqual((fresh["branch"], fresh["created"]), (f"review/{fid}/2", True))
        self.assertFalse((Path(fresh["path"]) / "a.txt").exists(), "the new decision starts from master")

    def test_worktree_is_reused_and_recreated_on_the_same_branch(self):
        fid, wt = self.decided()
        self.commit_in(wt, "a.txt", "work in progress\n")
        self.assertEqual(self.cli("worktree", fid), {"id": fid, "branch": f"review/{fid}/1", "path": str(wt.resolve()),
                                                     "created": False})
        self.git("worktree", "remove", str(wt))
        again = self.cli("worktree", fid)
        self.assertTrue(again["created"])
        self.assertEqual((Path(again["path"]) / "a.txt").read_text(), "work in progress\n")


class PayloadTest(StoreTest):
    def test_refuses_malformed_findings(self):
        cases = {
            "not a list": {"a": 1},
            "missing field": [{k: v for k, v in finding().items() if k != "evidence"}],
            "unknown key": [finding(rank=1)],
            "bad category": [finding(category="style")],
            "proof kind wrong for category": [finding(category="simplification")],
            "multi-line title": [finding(title="a\nb")],
            "empty locations": [finding(locations=[])],
        }
        for name, payload in cases.items():
            with self.subTest(name):
                self.refused("ingest", RUN, "--input", "-", payload=payload)
        self.assertEqual(self.cli("list", RUN), [])

    def test_refuses_malformed_pitches(self):
        (fid,) = self.ingest(finding())
        self.cli("verify", "--input", "-", payload=[{"id": fid, "verdict": "verified", "reason": "r"}])
        cases = {
            "ideal is not a candidate": {**pitch(fid), "ideal": "nope"},
            "no rejected solution": {**pitch(fid), "rejected": []},
            "recommendation is not a candidate": {**pitch(fid), "recommendation": {"key": "x", "justification": "j"}},
            "duplicate candidate keys": pitch(fid, keys=("same", "same")),
        }
        for name, payload in cases.items():
            with self.subTest(name):
                self.refused("pitch", "--input", "-", payload=[payload])
        self.assertEqual(self.state(fid), "verified")


if __name__ == "__main__":
    unittest.main()
