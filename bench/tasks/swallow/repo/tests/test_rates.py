from tax.rates import rate


def test_known():
    assert rate("DE") == 0.19


def test_unknown_region_uses_default():
    assert rate("XX") == 0.0
