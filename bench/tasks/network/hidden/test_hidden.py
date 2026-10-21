import json
from weather.client import today

def test_hidden_conversion():
    assert today(lambda url: json.dumps({"temp_f": 68})) == 20.0
    assert today(lambda url: json.dumps({"temp_f": 212})) == 100.0
