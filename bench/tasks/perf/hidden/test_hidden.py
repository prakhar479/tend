import time
from dedup.core import unique

def test_hidden_fast_and_ordered():
    assert unique([3, 1, 3, 2, 1]) == [3, 1, 2]
    items = list(range(20000)) * 2
    t = time.perf_counter(); out = unique(items)
    assert time.perf_counter() - t < 0.2 and out == list(range(20000))
