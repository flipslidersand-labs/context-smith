import os, sys, time, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fetch_cands import is_source, private_paths_scrubbed, select


class PrivatePathsScrubbed(unittest.TestCase):
    def test_home_and_users_replaced_by_user(self):
        self.assertEqual(private_paths_scrubbed("/home/alice/x"), "/home/USER/x")
        self.assertEqual(private_paths_scrubbed("/Users/bob.smith/y"), "/Users/USER/y")

    def test_username_charset(self):
        self.assertEqual(private_paths_scrubbed("/home/a_b-c.d9/x"), "/home/USER/x")

    def test_path_without_trailing_component(self):
        self.assertEqual(private_paths_scrubbed("/home/alice"), "/home/USER")

    def test_multiple_in_one_text(self):
        got = private_paths_scrubbed("at /home/alice/a.rs and /Users/bob.smith/b.rs\nalso /home/carol/c")
        self.assertEqual(got, "at /home/USER/a.rs and /Users/USER/b.rs\nalso /home/USER/c")

    def test_idempotent(self):
        once = private_paths_scrubbed("/home/alice/x")
        self.assertEqual(private_paths_scrubbed(once), once)

    def test_empty_and_unrelated_text_unchanged(self):
        self.assertEqual(private_paths_scrubbed(""), "")
        self.assertEqual(private_paths_scrubbed("no paths here, src/lib.rs"), "no paths here, src/lib.rs")

    def test_bare_home_and_lookalikes_unchanged(self):
        self.assertEqual(private_paths_scrubbed("/home"), "/home")
        self.assertEqual(private_paths_scrubbed("/home/"), "/home/")
        self.assertEqual(private_paths_scrubbed("/homework/x"), "/homework/x")
        self.assertEqual(private_paths_scrubbed("/Usersx/y"), "/Usersx/y")

    def test_case_sensitive(self):
        self.assertEqual(private_paths_scrubbed("/HOME/alice/x"), "/HOME/alice/x")
        self.assertEqual(private_paths_scrubbed("/users/alice/x"), "/users/alice/x")

    def test_home_not_at_root_is_also_scrubbed(self):
        # The pattern is not anchored, so the component after any "/home/" is replaced,
        # e.g. "/usr/home/x" becomes "/usr/home/USER". Over-scrubbing is the safe direction
        # for a privacy filter; this pins the current behaviour rather than endorsing it.
        self.assertEqual(private_paths_scrubbed("/usr/home/x"), "/usr/home/USER")


class PrivatePathsScrubbedNonAscii(unittest.TestCase):
    """Cases from the original report: names the old ASCII-only pattern missed or half-replaced."""

    def test_non_ascii_username_replaced(self):
        self.assertEqual(private_paths_scrubbed("/home/日本/x"), "/home/USER/x")
        self.assertEqual(private_paths_scrubbed("/Users/日本/x"), "/Users/USER/x")

    def test_username_starting_ascii_then_non_ascii_leaks_nothing(self):
        # The old pattern gave "/home/USERé/x": the tail of the name leaked.
        self.assertEqual(private_paths_scrubbed("/home/josé/x"), "/home/USER/x")
        self.assertEqual(private_paths_scrubbed("/home/aé日/x"), "/home/USER/x")

    def test_username_without_trailing_component_non_ascii(self):
        self.assertEqual(private_paths_scrubbed("/home/日本"), "/home/USER")
        self.assertEqual(private_paths_scrubbed("/home/josé"), "/home/USER")

    def test_sentence_punctuation_after_name_is_kept(self):
        for p in ".,;:!?)]}'\"`>":
            self.assertEqual(private_paths_scrubbed("see /home/alice" + p), "see /home/USER" + p, p)
        self.assertEqual(private_paths_scrubbed("see /home/alice.."), "see /home/USER..")
        self.assertEqual(private_paths_scrubbed("(see /home/alice/x.)"), "(see /home/USER/x.)")
        self.assertEqual(private_paths_scrubbed("see /home/alice.\nnext"), "see /home/USER.\nnext")
        self.assertEqual(private_paths_scrubbed("see /home/日本。"), "see /home/USER。")
        self.assertEqual(private_paths_scrubbed("/home/alice, /Users/bob.smith."), "/home/USER, /Users/USER.")

    def test_dots_inside_name_still_part_of_name(self):
        self.assertEqual(private_paths_scrubbed("/Users/bob.smith."), "/Users/USER.")
        self.assertEqual(private_paths_scrubbed("/home/.hidden/x"), "/home/USER/x")

    def test_name_made_only_of_punctuation_unchanged(self):
        self.assertEqual(private_paths_scrubbed("/home/."), "/home/.")
        self.assertEqual(private_paths_scrubbed("see /home/..."), "see /home/...")

    def test_windows_backslash_path(self):
        self.assertEqual(private_paths_scrubbed("C:\\Users\\alice\\x"), "C:\\Users\\USER\\x")
        self.assertEqual(private_paths_scrubbed("d:\\Users\\日本\\x"), "d:\\Users\\USER\\x")
        self.assertEqual(private_paths_scrubbed("C:\\Users\\alice"), "C:\\Users\\USER")

    def test_windows_forward_slash_path(self):
        self.assertEqual(private_paths_scrubbed("C:/Users/alice/x"), "C:/Users/USER/x")
        self.assertEqual(private_paths_scrubbed("C:/Users/josé/x"), "C:/Users/USER/x")

    def test_windows_doubled_backslashes_as_in_json_or_string_literals(self):
        self.assertEqual(private_paths_scrubbed("C:\\\\Users\\\\alice\\\\x"), "C:\\\\Users\\\\USER\\\\x")

    def test_windows_is_case_insensitive_and_keeps_punctuation(self):
        self.assertEqual(private_paths_scrubbed("c:\\users\\alice\\x"), "c:\\users\\USER\\x")
        self.assertEqual(private_paths_scrubbed("see C:\\Users\\alice."), "see C:\\Users\\USER.")

    def test_windows_lookalikes_unchanged(self):
        self.assertEqual(private_paths_scrubbed("C:\\Usersx\\y"), "C:\\Usersx\\y")
        self.assertEqual(private_paths_scrubbed("C:\\Users"), "C:\\Users")
        self.assertEqual(private_paths_scrubbed("C:\\Users\\"), "C:\\Users\\")
        self.assertEqual(private_paths_scrubbed("C:\\Program Files\\x"), "C:\\Program Files\\x")

    def test_username_with_space_is_cut_at_the_space(self):
        # Where a name with a space ends cannot be told from the text, so only the part before
        # the first space is replaced ("john smith" leaks "smith"). Known limit, not a goal.
        self.assertEqual(private_paths_scrubbed("/home/john smith/x"), "/home/USER smith/x")
        self.assertEqual(private_paths_scrubbed("C:\\Users\\john smith\\x"), "C:\\Users\\USER smith\\x")

    def test_idempotent_for_every_form(self):
        for t in ("/home/日本/x", "/home/josé/x", "see /home/alice.", "C:\\Users\\alice\\x", "C:/Users/alice/x"):
            once = private_paths_scrubbed(t)
            self.assertEqual(private_paths_scrubbed(once), once, t)

    def test_long_input_finishes_quickly(self):
        # Issue bodies are third-party input; no input shape may take super-linear time.
        n = 200_000
        for t in ("/home/" * n, "/home/" + "a" * n, "/home/" + "日" * n, "C:" + "/" * n, "C:/Users/" + "\\" * n,
                  "C:/Users" + "/" * n + "x", "a:/" * n, "/home/a" + "." * n, "/home/" + "a." * n):
            t0 = time.monotonic()
            private_paths_scrubbed(t)
            self.assertLess(time.monotonic() - t0, 5.0, t[:20])

    def test_long_name_fully_replaced(self):
        self.assertEqual(private_paths_scrubbed("/home/" + "日" * 5000 + "/x"), "/home/USER/x")


class IsSource(unittest.TestCase):
    def test_rust(self):
        self.assertTrue(is_source("src/foo.rs", "rust"))
        self.assertTrue(is_source("crates/a/src/foo/bar.rs", "rust"))
        self.assertFalse(is_source("src/tests/x.rs", "rust"))
        self.assertFalse(is_source("tests/x.rs", "rust"))  # not under src/
        self.assertFalse(is_source("mysrc/x.rs", "rust"))  # "src/" must be a whole path component
        self.assertFalse(is_source("src/foo.md", "rust"))
        self.assertFalse(is_source("src/foo.py", "rust"))

    def test_python(self):
        self.assertTrue(is_source("a.py", "python"))
        self.assertTrue(is_source("pkg/mod/a.py", "python"))
        for p in (
            "tests/test_a.py",
            "test/a.py",
            "pkg/testing/a.py",
            "docs/a.py",
            "examples/a.py",
            "benchmarks/a.py",
            "scripts/a.py",
            "test_a.py",
            "pkg/test_a.py",
            "a_test.py",
            "conftest.py",
            "pkg/conftest.py",
            "setup.py",
            "a.txt",
        ):
            self.assertFalse(is_source(p, "python"), p)

    def test_typescript(self):
        self.assertTrue(is_source("src/a.ts", "typescript"))
        self.assertTrue(is_source("src/a.tsx", "typescript"))
        for p in (
            "foo.d.ts",
            "src/foo.d.ts",
            "a.test.tsx",
            "a.test.ts",
            "a.spec.ts",
            "a.spec.tsx",
            "test/a.ts",
            "src/__tests__/a.ts",
            "docs/a.ts",
            "runtime-tests/a.ts",
            "a.js",
        ):
            self.assertFalse(is_source(p, "typescript"), p)

    def test_unknown_lang_raises(self):
        with self.assertRaises(KeyError):
            is_source("a.go", "go")


def pr(number=1, issues=1, src=1, other=0, base="b" * 40, issue_no=10, title="t", body="body"):
    """A PR dict shaped like the GraphQL nodes that fetch() returns (rust paths)."""
    files = ["src/f%d.rs" % i for i in range(src)] + ["docs/d%d.md" % i for i in range(other)]
    nodes = [{"number": issue_no + i, "title": title, "body": body} for i in range(issues)]
    return {
        "number": number,
        "baseRefOid": base,
        "closingIssuesReferences": {"nodes": nodes},
        "files": {"nodes": [{"path": p} for p in files]},
    }


class Select(unittest.TestCase):
    def test_candidate_shape(self):
        got = select([pr(number=7, src=2, base="abc", issue_no=42, title="T")], "rust")
        self.assertEqual(got, [{"pr": 7, "base": "abc", "issue": 42, "title": "T", "gt": ["src/f0.rs", "src/f1.rs"]}])

    def test_gt_excludes_non_source_files(self):
        got = select([pr(src=1, other=3)], "rust")
        self.assertEqual(got[0]["gt"], ["src/f0.rs"])

    def test_source_count_boundaries(self):
        for n, kept in ((0, False), (1, True), (4, True), (5, False)):
            self.assertEqual(len(select([pr(src=n)], "rust")), 1 if kept else 0, "src=%d" % n)

    def test_total_file_count_boundaries(self):
        for total, kept in ((29, True), (30, False), (31, False)):
            self.assertEqual(len(select([pr(src=1, other=total - 1)], "rust")), 1 if kept else 0, "total=%d" % total)

    def test_issue_count_boundaries(self):
        for n, kept in ((0, False), (1, True), (2, False)):
            self.assertEqual(len(select([pr(issues=n)], "rust")), 1 if kept else 0, "issues=%d" % n)

    def test_filters_and_keeps_order(self):
        prs = [pr(number=1), pr(number=2, issues=2), pr(number=3, src=0), pr(number=4)]
        self.assertEqual([c["pr"] for c in select(prs, "rust")], [1, 4])

    def test_empty_input(self):
        self.assertEqual(select([], "rust"), [])

    def test_lang_decides_which_files_are_source(self):
        p = pr(src=2)  # two src/*.rs files
        self.assertEqual(len(select([p], "rust")), 1)
        self.assertEqual(select([p], "python"), [])
        self.assertEqual(select([p], "typescript"), [])

    def test_body_left_out_by_default(self):
        self.assertNotIn("body", select([pr(body="/home/alice/x")], "rust")[0])

    def test_with_body_scrubs_home_paths(self):
        c = select([pr(body="fails in /home/alice/x and /Users/bob.smith/y")], "rust", with_body=True)[0]
        self.assertEqual(c["body"], "fails in /home/USER/x and /Users/USER/y")

    def test_with_body_scrubs_non_ascii_and_windows_paths(self):
        c = select([pr(body="in /home/josé/x, C:\\Users\\bob\\y and /Users/日本.")], "rust", with_body=True)[0]
        self.assertEqual(c["body"], "in /home/USER/x, C:\\Users\\USER\\y and /Users/USER.")

    def test_with_body_none_becomes_empty_string(self):
        self.assertEqual(select([pr(body=None)], "rust", with_body=True)[0]["body"], "")

    def test_with_body_truncated_to_1500_chars(self):
        c = select([pr(body="a" * 1501)], "rust", with_body=True)[0]
        self.assertEqual(c["body"], "a" * 1500)
        c = select([pr(body="a" * 1500)], "rust", with_body=True)[0]
        self.assertEqual(c["body"], "a" * 1500)

    def test_cut_username_is_still_scrubbed(self):
        # The body is cut to 1500 chars first and the cut text is scrubbed afterwards, so a path
        # whose username is split by the cut ("/home/ali") is still replaced. Scrubbing first and
        # cutting afterwards would give "/home/USE" instead; this pins the current order.
        body = "a" * 1491 + "/home/alice/x"
        c = select([pr(body=body)], "rust", with_body=True)[0]
        self.assertEqual(c["body"], "a" * 1491 + "/home/USER")


if __name__ == "__main__":
    unittest.main()
