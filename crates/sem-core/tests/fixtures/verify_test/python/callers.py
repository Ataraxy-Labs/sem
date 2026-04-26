from functions import Database, create_user, delete_user, find_users, log_message


def good_caller():
    db = Database()
    db.connect("localhost", 5432)
    db.query("SELECT 1")
    db.query("SELECT ?", [1])
    db.close()
    create_user("alice", "alice@example.com", 30)
    delete_user(42)
    find_users("alice")
    find_users("alice", 20)
    find_users("alice", 20, 5)
    log_message("hello", "world", extra=True)


def bad_caller_too_few():
    create_user("alice")


def bad_caller_too_many():
    delete_user(42, "extra_arg")


def bad_caller_method():
    db = Database()
    db.connect("localhost")
