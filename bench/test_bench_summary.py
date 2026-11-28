"""bench.py must stop with a clear message when no task was measured (n == 0), not with ZeroDivisionError.

Runs bench.py as a subprocess against a throwaway non-git directory and candidate files in a tmp dir:
no clone, no contextsmith binary and no network are needed (every task is skipped at the checkout step).
Run from bench/: python3 -m unittest test_bench_summary
"""
import json, os, subprocess, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def run_bench(cands, *args):
    with tempfile.TemporaryDirectory() as d:
        repo = os.path.join(d, "repo")
        os.mkdir(repo)  # not a git repo: `git checkout` fails, so every task is skipped
        cpath = os.path.join(d, "cands.json")
        with open(cpath, "w") as f:
            json.dump(cands, f)
        env = dict(os.environ, REPO=repo, CANDS=cpath, TASKMODE="title")
        return subprocess.run([sys.executable, os.path.join(HERE, "bench.py"), *args], capture_output=True, text=True, env=env)


TASK = {"pr": 1, "base": "deadbeef", "title": "t", "gt": ["src/a.rs"]}


class NothingMeasured(unittest.TestCase):
    def test_all_tasks_skipped_exits_with_message(self):
        r = run_bench([TASK, dict(TASK, pr=2)])
        self.assertNotIn("ZeroDivisionError", r.stderr)
        self.assertEqual(r.returncode, 1)
        self.assertIn("no task measured", r.stderr)
        self.assertIn("skipped 2", r.stderr)
        self.assertIn("cs query errors 0", r.stderr)

    def test_limit_zero_exits_with_message(self):
        r = run_bench([TASK], "0")
        self.assertNotIn("ZeroDivisionError", r.stderr)
        self.assertEqual(r.returncode, 1)
        self.assertIn("no task measured (skipped 0, cs query errors 0)", r.stderr)

    def test_empty_candidates_exits_with_message(self):
        r = run_bench([])
        self.assertNotIn("ZeroDivisionError", r.stderr)
        self.assertIn("no task measured", r.stderr)


if __name__ == "__main__":
    unittest.main()
