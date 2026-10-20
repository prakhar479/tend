import os
from pathlib import Path

from report.table import render

GOLDEN = Path(__file__).parent / "golden" / "table.txt"


def test_table_matches_golden():
    out = render([("apple", 3), ("banana", 12)])
    if os.environ.get("UPDATE_GOLDEN") == "1":
        GOLDEN.write_text(out)
    assert out == GOLDEN.read_text()
