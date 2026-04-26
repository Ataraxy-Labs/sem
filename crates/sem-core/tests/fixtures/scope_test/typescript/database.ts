export class Connection {
    execute(query: string): any {
        return null;
    }

    commit(): void {}

    close(): void {}
}

export class Transaction {
    private conn: Connection;

    constructor(conn: Connection) {
        this.conn = conn;
    }

    execute(query: string): any {
        return this.conn.execute(query);
    }

    commit(): void {
        this.conn.commit();
    }

    rollback(): void {}
}

export function getConnection(): Connection {
    return new Connection();
}
