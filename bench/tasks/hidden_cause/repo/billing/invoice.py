from billing.config import CENTS_PER_UNIT


def to_cents(amount):
    return round(amount * CENTS_PER_UNIT)


def invoice_total(lines):
    """Total in cents of (quantity, unit price in currency units) lines."""
    return sum(to_cents(q * p) for q, p in lines)
