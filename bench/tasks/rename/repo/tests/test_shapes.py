import math

from geo.shapes import area_of_circle, area_of_rect


def test_circle():
    assert area_of_circle(2) == math.pi * 4


def test_rect():
    assert area_of_rect(2, 3) == 6
