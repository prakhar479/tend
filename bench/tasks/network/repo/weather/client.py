import json
import urllib.request

API = "http://weather.invalid/api/today"


def today(fetch=None):
    """Today's temperature in Celsius. `fetch(url) -> str` returns the API body."""
    body = (fetch or _get)(API)
    return round(json.loads(body)["temp_f"] - 32, 1)


def _get(url):
    with urllib.request.urlopen(url, timeout=2) as r:
        return r.read().decode()
