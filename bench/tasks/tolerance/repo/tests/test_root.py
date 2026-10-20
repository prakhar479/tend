import pytest

from num.root import sqrt


@pytest.mark.parametrize("x", [2.0, 10.0, 12345.0, 0.25])
def test_sqrt_is_precise(x):
    assert sqrt(x) == pytest.approx(x ** 0.5, rel=1e-9)
