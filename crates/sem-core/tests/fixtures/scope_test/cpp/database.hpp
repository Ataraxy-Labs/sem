#include <string>

class Connection {
public:
    void* execute(std::string query) {
        return nullptr;
    }

    void commit() {
    }

    void close() {
    }
};

class Transaction {
private:
    Connection* conn;
public:
    Transaction(Connection* conn) : conn(conn) {}

    void* execute(std::string query) {
        return conn->execute(query);
    }

    void commit() {
        conn->commit();
    }

    void rollback() {
    }
};

Connection* getConnection() {
    static Connection conn;
    return &conn;
}
