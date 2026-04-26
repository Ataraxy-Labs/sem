"""Name collisions and aliased imports."""

from project.core import validate as core_validate
from project.core import transform


def validate(request):
    """Local validate that shadows the import name."""
    return request is not None


def handle_request(request):
    """Uses the aliased import (core_validate) and local validate."""
    if validate(request):
        return core_validate(request["data"])
    return None


def handle_transform(data):
    """Uses cross-file import directly."""
    return transform(data)
