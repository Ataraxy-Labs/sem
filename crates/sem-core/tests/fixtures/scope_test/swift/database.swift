class Connection {
    func execute(query: String) -> Any? {
        return nil
    }

    func commit() {}

    func close() {}
}

class Logger {
    func record(message: String) {}
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

class Replicator {
    var primary, backup: Connection

    init(primary: Connection, backup: Connection) {
        self.primary = primary
        self.backup = backup
    }

    func sync() {
        primary.execute(query: "SELECT 1")
        backup.commit()
    }
}

class AuditedTransaction {
    var conn: Connection, logger: Logger

    init(conn: Connection, logger: Logger) {
        self.conn = conn
        self.logger = logger
    }

    func write() {
        conn.commit()
        logger.record(message: "done")
    }
}

func getConnection() -> Connection {
    return Connection()
}
