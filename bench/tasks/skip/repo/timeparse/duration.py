import re


def seconds(text):
    """Parse durations like '90s', '15m', '1h30m', '2h' into seconds."""
    total = 0
    for value, unit in re.findall(r"(\d+)([sm])", text):
        total += int(value) * {"s": 1, "m": 60}[unit]
    return total
