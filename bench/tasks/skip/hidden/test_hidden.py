from timeparse.duration import seconds

def test_hidden_hours():
    assert seconds("1h30m") == 5400
    assert seconds("2h") == 7200
    assert seconds("1h1m1s") == 3661
