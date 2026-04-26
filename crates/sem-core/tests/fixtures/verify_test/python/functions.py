class Database:
    def connect(self, host, port):
        pass

    def query(self, sql, params=None):
        pass

    def close(self):
        pass


def create_user(name, email, age):
    pass


def delete_user(user_id):
    pass


def find_users(query, limit=10, offset=0):
    pass


def log_message(*args, **kwargs):
    pass
