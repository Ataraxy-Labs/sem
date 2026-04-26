"""Re-exports from other modules."""

from project.core import validate, sanitize
from project.utils import expensive_compute


def validate_and_compute(data, n):
    """Combines validation with computation."""
    if validate(data):
        return expensive_compute(n)
    return None
