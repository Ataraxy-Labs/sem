"""Utility decorators."""

import functools


def memoize(func):
    """Cache results of a function call."""
    cache = {}

    @functools.wraps(func)
    def wrapper(*args):
        if args not in cache:
            cache[args] = func(*args)
        return cache[args]

    return wrapper


@memoize
def expensive_compute(n):
    """A decorated function."""
    total = 0
    for i in range(n):
        total += i * i
    return total
