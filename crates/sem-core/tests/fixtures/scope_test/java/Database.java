public class Connection {
    public Object execute(String query) {
        return null;
    }

    public void commit() {
    }

    public void close() {
    }
}

class Transaction {
    private Connection conn;

    public Transaction(Connection conn) {
        this.conn = conn;
    }

    public Object execute(String query) {
        return conn.execute(query);
    }

    public void commit() {
        conn.commit();
    }

    public void rollback() {
    }
}

class DatabaseHelper {
    public static Connection getConnection() {
        return new Connection();
    }
}
