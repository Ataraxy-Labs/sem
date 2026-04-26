fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("sem-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    sem_mcp::run()
}
