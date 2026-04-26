"""Core validation and sanitization functions."""


def sanitize(text):
    """Remove dangerous characters from text."""
    return text.replace("<", "").replace(">", "")


def validate(data):
    """Validate input data by sanitizing it first."""
    cleaned = sanitize(data)
    return len(cleaned) > 0


def transform(data):
    """Transform data after validation."""
    if validate(data):
        return data.upper()
    return data
