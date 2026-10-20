def page(items, start, size):
    """Items from `start`, at most `size` of them."""
    end = min(start + size, len(items) - 1)
    return items[start:end]
