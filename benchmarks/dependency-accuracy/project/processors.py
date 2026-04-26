"""Higher-order functions and callbacks."""

from project.core import sanitize


def apply_to_all(items, func):
    """Apply a function to all items."""
    return [func(item) for item in items]


def process_batch(items):
    """Process a batch by applying sanitize to all items."""
    return apply_to_all(items, sanitize)
