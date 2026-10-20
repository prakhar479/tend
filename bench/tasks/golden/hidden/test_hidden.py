from report.table import render

def test_hidden_right_aligned():
    assert render([("apple", 3), ("banana", 12)]) == 'name        qty\napple         3\nbanana       12\n'
    assert render([("kiwi", 100)]).splitlines()[1] == 'kiwi        100'
