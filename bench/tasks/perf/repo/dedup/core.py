def unique(items):
    """Items in first-seen order, without duplicates."""
    out = []
    for x in items:
        if x not in out:
            out.append(x)
    return out
