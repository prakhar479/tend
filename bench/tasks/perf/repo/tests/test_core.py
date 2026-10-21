import time

from dedup.core import unique


def test_order_kept():
    assert unique([3, 1, 3, 2, 1]) == [3, 1, 2]


def test_fast_on_large_input():
    items = list(range(20000)) * 2
    start = time.perf_counter()
    unique(items)
    assert time.perf_counter() - start < 0.2
