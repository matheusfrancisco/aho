# intervals

A small library for closed integer intervals. Implement it in `intervals/__init__.py`.

An interval is a pair `(start, end)` of integers meaning every point `x` with
`start <= x <= end`. A zero-length interval such as `(3, 3)` is valid; `(5, 1)` is not.

## `merge(intervals) -> list[tuple[int, int]]`

- `intervals` is any iterable of `(start, end)` pairs, in any order.
- Returns the smallest list of intervals covering exactly the same points, sorted by
  `start`, as a list of tuples.
- Intervals that overlap or touch (one ends exactly where the next starts, e.g. `(1, 3)` and
  `(3, 5)`) are merged into one. Intervals that merely sit next to each other on the integer
  line, e.g. `(1, 2)` and `(3, 4)`, are not merged.
- The argument is never modified.
- An empty input returns `[]`.
- Raises `ValueError` if any interval has `start > end`.

## `total_length(intervals) -> int`

- The total length covered by the union of `intervals`, where the length of `(start, end)` is
  `end - start`. Points covered by several intervals count once.
- An empty input returns `0`.
- Raises `ValueError` if any interval has `start > end`.

## Examples

```python
merge([(8, 10), (1, 3), (2, 6), (15, 18)])  # [(1, 6), (8, 10), (15, 18)]
merge([(1, 3), (3, 5)])                      # [(1, 5)]
total_length([(1, 3), (2, 5), (10, 11)])    # 5
```
