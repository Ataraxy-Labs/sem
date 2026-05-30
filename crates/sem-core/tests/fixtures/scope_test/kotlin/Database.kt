class Connection {
    fun execute(query: String): Any? {
        return null
    }

    fun commit() {}

    fun close() {}
}

class Transaction(val conn: Connection) {
    fun execute(query: String): Any? {
        return conn.execute(query)
    }

    fun commit() {
        conn.commit()
    }

    fun rollback() {}
}

fun getConnection(): Connection {
    return Connection()
}
