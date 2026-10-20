from shop.pricing import total


def test_no_discount():
    assert total([10.0, 20.0]) == 30.0


def test_discount_rounds_once_at_the_end():
    # 3 x 33.33 with 10% off: 89.991, rounded once -> 89.99
    assert total([33.33, 33.33, 33.33], 10) == 89.99
