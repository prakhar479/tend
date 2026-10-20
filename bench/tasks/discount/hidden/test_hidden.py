from shop.pricing import total

def test_hidden_round_once():
    assert total([33.33, 33.33, 33.33], 10) == 89.99
    assert total([19.99, 5.01], 50) == 12.5
