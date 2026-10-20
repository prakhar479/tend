from billing.invoice import invoice_total


def test_invoice_total_in_cents():
    assert invoice_total([(2, 1.50), (1, 0.25)]) == 325
