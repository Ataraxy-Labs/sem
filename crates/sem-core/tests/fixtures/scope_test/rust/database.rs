pub struct Connection {
    pub active: bool,
}

impl Connection {
    pub fn execute(&self, query: &str) -> Vec<String> {
        Vec::new()
    }

    pub fn commit(&self) {}

    pub fn close(&mut self) {
        self.active = false;
    }
}

pub struct Transaction {
    pub conn: Connection,
}

impl Transaction {
    pub fn new(conn: Connection) -> Transaction {
        Transaction { conn }
    }

    pub fn execute(&self, query: &str) -> Vec<String> {
        self.conn.execute(query)
    }

    pub fn commit(&self) {
        self.conn.commit()
    }

    pub fn rollback(&self) {}
}

pub fn get_connection() -> Connection {
    Connection { active: true }
}
