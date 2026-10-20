from timeparse.duration import seconds


def test_seconds_and_minutes():
    assert seconds("90s") == 90
    assert seconds("15m") == 900


def test_hours():
    assert seconds("1h30m") == 5400
    assert seconds("2h") == 7200
