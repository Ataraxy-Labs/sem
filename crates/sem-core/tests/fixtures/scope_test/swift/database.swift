class Connection {
    func execute(query: String) -> Any? {
        return nil
    }

    func commit() {}

    func close() {}
}

class Transaction {
    var conn: Connection

    init(conn: Connection) {
        self.conn = conn
    }

    func execute(query: String) -> Any? {
        return conn.execute(query: query)
    }

    func commit() {
        conn.commit()
    }

    func rollback() {}
}

func getConnection() -> Connection {
    return Connection()
}
