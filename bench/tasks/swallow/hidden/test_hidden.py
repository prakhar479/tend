from tax.rates import rate

def test_hidden_default():
    assert rate("XX") == 0.0
    assert rate("FR") == 0.20
