def render(rows):
    """Render (name, qty) rows as a fixed-width table: name left in 10, qty right in 5."""
    lines = [f"{'name':<10}{'qty':>5}"]
    for name, qty in rows:
        lines.append(f"{name:<10}{qty:<5}")
    return "\n".join(lines) + "\n"
