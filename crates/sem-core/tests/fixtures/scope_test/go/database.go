package database

type Connection struct {
	Active bool
}

func (c *Connection) Execute(query string) []string {
	return nil
}

func (c *Connection) Commit() {}

func (c *Connection) Close() {
	c.Active = false
}

type Transaction struct {
	Conn *Connection
}

func NewTransaction(conn *Connection) *Transaction {
	return &Transaction{Conn: conn}
}

func (t *Transaction) Execute(query string) []string {
	return t.Conn.Execute(query)
}

func (t *Transaction) Commit() {
	t.Conn.Commit()
}

func (t *Transaction) Rollback() {}

func GetConnection() *Connection {
	return &Connection{Active: true}
}
