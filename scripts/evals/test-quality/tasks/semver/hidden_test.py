import unittest

from semver import compare

CHAIN = [
    "1.0.0-alpha",
    "1.0.0-alpha.1",
    "1.0.0-alpha.beta",
    "1.0.0-beta",
    "1.0.0-beta.2",
    "1.0.0-beta.11",
    "1.0.0-rc.1",
    "1.0.0",
]

INVALID = [
    "",
    "1",
    "1.2",
    "1.2.3.4",
    "01.0.0",
    "1.02.0",
    "1.0.0-",
    "1.0.0-01",
    "1.0.0-a..b",
    "1.0.0-al$pha",
    "1.0.0+",
    "a.b.c",
    "v1.0.0",
]


class Compare(unittest.TestCase):
    def test_equal(self):
        self.assertEqual(compare("1.2.3", "1.2.3"), 0)

    def test_core_numbers_in_order(self):
        self.assertEqual(compare("1.0.0", "2.0.0"), -1)
        self.assertEqual(compare("2.1.0", "2.0.9"), 1)
        self.assertEqual(compare("1.0.1", "1.0.0"), 1)

    def test_core_numbers_compare_numerically(self):
        self.assertEqual(compare("1.9.0", "1.10.0"), -1)
        self.assertEqual(compare("10.0.0", "9.0.0"), 1)

    def test_result_is_exactly_one_step(self):
        self.assertEqual(compare("1.0.0", "3.0.0"), -1)
        self.assertEqual(compare("3.0.0", "1.0.0"), 1)

    def test_prerelease_ranks_below_release(self):
        self.assertEqual(compare("1.0.0-alpha", "1.0.0"), -1)
        self.assertEqual(compare("1.0.0", "1.0.0-alpha"), 1)

    def test_core_beats_prerelease(self):
        self.assertEqual(compare("1.0.1-alpha", "1.0.0"), 1)

    def test_spec_chain(self):
        for lower, higher in zip(CHAIN, CHAIN[1:]):
            with self.subTest(lower=lower, higher=higher):
                self.assertEqual(compare(lower, higher), -1)
                self.assertEqual(compare(higher, lower), 1)

    def test_longer_prerelease_ranks_higher(self):
        self.assertEqual(compare("1.0.0-alpha.1", "1.0.0-alpha"), 1)

    def test_build_metadata_ignored(self):
        self.assertEqual(compare("1.0.0+build.1", "1.0.0+other"), 0)
        self.assertEqual(compare("1.0.0-alpha+001", "1.0.0-alpha"), 0)

    def test_hyphens_inside_identifiers(self):
        self.assertEqual(compare("1.0.0-x-y", "1.0.0-x-z"), -1)

    def test_invalid_versions_rejected(self):
        for bad in INVALID:
            with self.subTest(version=bad):
                with self.assertRaises(ValueError):
                    compare(bad, "1.0.0")
                with self.assertRaises(ValueError):
                    compare("1.0.0", bad)


if __name__ == "__main__":
    unittest.main()
