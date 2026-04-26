from api import create_user, create_admin, list_users
from auth import User


def handle_signup(request):
    name = request.get("name")
    email = request.get("email")
    return create_user(name, email)


def handle_admin_create(request):
    name = request.get("name")
    email = request.get("email")
    role = request.get("role", "admin")
    return create_admin(name, email, role)


def handle_list(request):
    users = list_users()
    return [u for u in users]


def validate_request(request):
    if not request.get("name"):
        raise ValueError("Name required")
    if not request.get("email"):
        raise ValueError("Email required")
    return True
