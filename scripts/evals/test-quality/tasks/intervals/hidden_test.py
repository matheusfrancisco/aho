import unittest

from intervals import merge, total_length


class Merge(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(merge([]), [])

    def test_single(self):
        self.assertEqual(merge([(1, 3)]), [(1, 3)])

    def test_disjoint_come_back_sorted(self):
        self.assertEqual(merge([(5, 6), (1, 2)]), [(1, 2), (5, 6)])

    def test_overlap_keeps_first_start(self):
        self.assertEqual(merge([(1, 4), (2, 6)]), [(1, 6)])

    def test_touching_merge(self):
        self.assertEqual(merge([(1, 3), (3, 5)]), [(1, 5)])

    def test_neighbours_on_the_integer_line_stay_apart(self):
        self.assertEqual(merge([(1, 2), (3, 4)]), [(1, 2), (3, 4)])

    def test_contained_interval_is_absorbed(self):
        self.assertEqual(merge([(1, 10), (2, 3)]), [(1, 10)])

    def test_unsorted_mix(self):
        self.assertEqual(merge([(8, 10), (1, 3), (2, 6), (15, 18)]), [(1, 6), (8, 10), (15, 18)])

    def test_zero_length(self):
        self.assertEqual(merge([(3, 3)]), [(3, 3)])
        self.assertEqual(merge([(1, 3), (3, 3)]), [(1, 3)])

    def test_negative_bounds(self):
        self.assertEqual(merge([(-5, -1), (-2, 0)]), [(-5, 0)])

    def test_any_iterable(self):
        self.assertEqual(merge(iter([(2, 3), (1, 2)])), [(1, 3)])

    def test_argument_not_modified(self):
        given = [(3, 4), (1, 2)]
        merge(given)
        self.assertEqual(given, [(3, 4), (1, 2)])

    def test_reversed_interval_rejected(self):
        with self.assertRaises(ValueError):
            merge([(1, 2), (5, 1)])


class TotalLength(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(total_length([]), 0)

    def test_overlaps_count_once(self):
        self.assertEqual(total_length([(1, 3), (2, 5), (10, 11)]), 5)

    def test_zero_length_adds_nothing(self):
        self.assertEqual(total_length([(3, 3)]), 0)

    def test_reversed_interval_rejected(self):
        with self.assertRaises(ValueError):
            total_length([(5, 1)])


if __name__ == "__main__":
    unittest.main()
