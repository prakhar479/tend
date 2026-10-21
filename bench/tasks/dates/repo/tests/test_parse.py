import datetime as dt

from when.parse import parse


def test_iso():
    assert parse("2024-02-29") == dt.date(2024, 2, 29)


def test_day_first():
    assert parse("29/02/2024") == dt.date(2024, 2, 29)
    assert parse("03/04/2024") == dt.date(2024, 4, 3)


def test_month_name():
    assert parse("Feb 29 2024") == dt.date(2024, 2, 29)
