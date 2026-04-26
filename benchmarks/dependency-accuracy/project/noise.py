"""False positive traps: mentions entity names but has no real dependencies.

This module talks about validate and sanitize in docstrings and comments,
but never actually imports or calls them. Any edges from this file are
false positives.
"""

# We should use validate() here but we don't
# sanitize would also be useful

from project.core import sanitize  # imported but never called


def unrelated_function():
    """This function does not call validate or sanitize.

    Note: validate is mentioned here only as documentation.
    The sanitize function from core could help but we chose not to use it.
    """
    return 42


def another_function():
    """Completely standalone."""
    return unrelated_function()
