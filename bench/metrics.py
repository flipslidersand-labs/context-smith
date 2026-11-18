"""Pure metric functions for bench.py (no I/O, no environment), unit-tested in test_metrics.py."""


def file_prf(returned, gold):
    """(precision, recall, f1) of the returned file set against the gold set.
    Empty returned set or empty gold set gives 0.0 for the undefined parts; f1 is 0 when p+r is 0."""
    returned, gold = set(returned), set(gold)
    hit = len(returned & gold)
    p = hit / len(returned) if returned else 0.0
    r = hit / len(gold) if gold else 0.0
    f1 = 2 * p * r / (p + r) if p + r else 0.0
    return p, r, f1


def budget_coverage(included, gold_sizes):
    """Share of the gold files' tokens that made it into the bundle.
    included: {path: tokens of that file actually in the bundle}; gold_sizes: {path: full est tokens}.
    Each gold file contributes min(included, size) / total gold size, so a half-truncated file counts half."""
    total = sum(gold_sizes.values())
    if total <= 0:
        return 0.0
    return sum(min(included.get(p, 0), s) for p, s in gold_sizes.items()) / total
