def total(prices, discount_pct=0):
    """Sum of prices after a percentage discount, rounded to cents."""
    # Each line is discounted and rounded on its own.
    return sum(round(p * (1 - discount_pct / 100), 2) for p in prices)
