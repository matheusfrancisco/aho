"""Closed integer intervals: see README.md."""


def _checked(intervals):
    pairs = []
    for item in intervals:
        start, end = item
        if start > end:
            raise ValueError(f"interval {item!r} has start > end")
        pairs.append((start, end))
    return pairs


def merge(intervals):
    pairs = sorted(_checked(intervals))
    out = []
    for start, end in pairs:
        if out and start <= out[-1][1]:
            if end > out[-1][1]:
                out[-1] = (out[-1][0], end)
        else:
            out.append((start, end))
    return out


def total_length(intervals):
    return sum(end - start for start, end in merge(intervals))
