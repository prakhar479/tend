def sqrt(x):
    """Square root by Newton's method."""
    if x == 0:
        return 0.0
    guess = x
    for _ in range(3):
        guess = (guess + x / guess) / 2
    return guess
