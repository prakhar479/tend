import math
from num.root import sqrt

def test_hidden_precise():
    for x in [2.0, 10.0, 12345.0, 0.25, 1e6, 7.5]:
        assert abs(sqrt(x) - math.sqrt(x)) <= 1e-9 * math.sqrt(x)
