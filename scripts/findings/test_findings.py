"""Tests for the finding store CLI (plan/2026-10-21-1047/PO1, plan/2026-10-21-1047/PO7).

Run: python3 -m unittest discover -s scripts/findings
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
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

    def run_cli(self, *args: str, payload=None) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        stdin = io.StringIO(json.dumps(payload)) if payload is not None else io.StringIO("")
        with mock.patch.object(sys, "stdin", stdin), contextlib.redirect_stdout(out), \
                contextlib.redirect_stderr(err):
            rc = findings.main(["--store", str(self.store), "--repo", str(self.repo), *args])
        return rc, out.getvalue(), err.getvalue()

    def cli(self, *args: str, payload=None):
        rc, out, err = self.run_cli(*args, payload=payload)
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
            (first["action"], first["branch"], first["candidate"]["key"], first["decision"]["note"]),
            ("start", f"review/{fid}/1", "local-fix", "keep the old flag"),
        )
        self.git("branch", first["branch"])
        self.assertEqual(self.cli("launch", fid)["action"], "resume")
        self.cli("block", fid, "--reason", "the flag cannot stay")
        self.assertEqual(self.cli("launch", fid)["action"], "stop", "a blocked branch is never resumed")
        self.cli("decide", fid, "--candidate", "ideal-fix")
        second = self.cli("launch", fid)
        self.assertEqual((second["action"], second["branch"], second["candidate"]["key"]),
                         ("start", f"review/{fid}/2", "ideal-fix"))
        self.land(fid)
        self.assertEqual(self.cli("launch", fid), {"id": fid, "action": "stop", "reason": "merged"})


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
