class Connection:
    def execute(self, query):
        pass

    def commit(self):
        pass

    def close(self):
        pass


class Transaction:
    def __init__(self, conn):
        self.conn = conn

    def execute(self, query):
        self.conn.execute(query)

    def commit(self):
        self.conn.commit()

    def rollback(self):
        pass


def get_connection():
    return Connection()
