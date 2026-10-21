import datetime as dt


def parse(text):
    """Dates as '2024-02-29', '29/02/2024', or 'Feb 29 2024'."""
    for fmt in ("%Y-%m-%d", "%m/%d/%Y", "%b %d %Y"):
        try:
            return dt.datetime.strptime(text, fmt).date()
        except ValueError:
            pass
    raise ValueError(text)
