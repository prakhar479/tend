import pytest

from pager.page import page


@pytest.mark.parametrize("n", range(1500))
def test_items_are_kept(n):
    assert n in [x for p in range(0, 1500, 50) for x in page(list(range(1500)), p, 50)]


def test_last_page_is_complete():
    items = list(range(105))
    assert page(items, 100, 10) == [100, 101, 102, 103, 104]
