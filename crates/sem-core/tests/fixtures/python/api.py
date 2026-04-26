from auth import User, Admin
from db import get_connection, save_record


def create_user(name, email):
    user = User(name, email)
    if not user.validate():
        raise ValueError("Invalid user")
    conn = get_connection()
    save_record(conn, user.to_dict())
    return user


def create_admin(name, email, role):
    admin = Admin(name, email, role)
    if not admin.validate():
        raise ValueError("Invalid admin")
    perms = admin.get_permissions()
    conn = get_connection()
    save_record(conn, {**admin.to_dict(), "permissions": perms})
    return admin


def list_users():
    conn = get_connection()
    return conn.execute("SELECT * FROM users")
