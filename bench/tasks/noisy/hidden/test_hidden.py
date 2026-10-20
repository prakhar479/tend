from pager.page import page

def test_hidden_page():
    assert page(list(range(105)), 100, 10) == [100, 101, 102, 103, 104]
    assert page(list(range(10)), 0, 10) == list(range(10))
