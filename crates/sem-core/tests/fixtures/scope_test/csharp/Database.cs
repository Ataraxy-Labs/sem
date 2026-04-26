public class Connection {
    public object Execute(string query) {
        return null;
    }

    public void Commit() {
    }

    public void Close() {
    }
}

public class Transaction {
    private Connection conn;

    public Transaction(Connection conn) {
        this.conn = conn;
    }

    public object Execute(string query) {
        return conn.Execute(query);
    }

    public void Commit() {
        conn.Commit();
    }

    public void Rollback() {
    }
}

public static class DatabaseHelper {
    public static Connection GetConnection() {
        return new Connection();
    }
}
