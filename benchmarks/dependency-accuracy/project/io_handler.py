"""Cross-file imports and function calls."""

from project.core import validate, sanitize
from project.models import UserModel


def process_input(raw_data):
    """Process raw input using core validation."""
    if validate(raw_data):
        clean = sanitize(raw_data)
        return clean
    return None


def create_user(name, email):
    """Create a user model from validated input."""
    clean_name = sanitize(name)
    return UserModel(clean_name, email)
