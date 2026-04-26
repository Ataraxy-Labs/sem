class Connection
  def execute(query)
    nil
  end

  def commit
  end

  def close
  end
end

class Transaction
  def initialize(conn)
    @conn = conn
  end

  def execute(query)
    @conn.execute(query)
  end

  def commit
    @conn.commit
  end

  def rollback
  end
end

def get_connection
  Connection.new
end
