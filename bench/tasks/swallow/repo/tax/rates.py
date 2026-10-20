RATES = {"DE": 0.19, "FR": 0.20}
DEFAULT_RATE = 0.0


def rate(region):
    """VAT rate for a region; unknown regions use DEFAULT_RATE."""
    return RATES[region]
