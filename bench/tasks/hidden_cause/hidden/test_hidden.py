from billing.invoice import invoice_total, to_cents

def test_hidden_cents():
    assert invoice_total([(2, 1.50), (1, 0.25)]) == 325
    assert to_cents(1) == 100
