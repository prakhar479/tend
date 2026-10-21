import datetime as dt
from when.parse import parse

def test_hidden_day_first():
    assert parse("03/04/2024") == dt.date(2024, 4, 3)
    assert parse("29/02/2024") == dt.date(2024, 2, 29)
    assert parse("Feb 29 2024") == dt.date(2024, 2, 29)
