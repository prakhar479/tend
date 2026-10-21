import math
from geo import shapes

def test_hidden_both_names_or_new():
    f = getattr(shapes, "circle_area", None) or getattr(shapes, "area_of_circle")
    g = getattr(shapes, "rect_area", None) or getattr(shapes, "area_of_rect")
    assert f(2) == math.pi * 4 and g(2, 3) == 6
