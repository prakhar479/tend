from weather.client import today


def test_today_in_celsius():
    assert today() == 20.0
