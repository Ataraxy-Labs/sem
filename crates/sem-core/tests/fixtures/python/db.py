import sqlite3


def get_connection():
    return sqlite3.connect("app.db")


def save_record(conn, data):
    keys = ", ".join(data.keys())
    placeholders = ", ".join(["?"] * len(data))
    conn.execute(
        f"INSERT INTO users ({keys}) VALUES ({placeholders})",
        list(data.values()),
    )
    conn.commit()


def delete_record(conn, user_id):
    conn.execute("DELETE FROM users WHERE id = ?", (user_id,))
    conn.commit()
