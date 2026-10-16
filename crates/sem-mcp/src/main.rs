#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("sem-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--http") {
        if args.len() != 2 {
            return Err("Usage: sem-mcp --http 127.0.0.1:8765".into());
        }
        return sem_mcp::http::run(args[1].parse()?);
    }
    if !args.is_empty() {
        return Err("Usage: sem-mcp [--http 127.0.0.1:8765]".into());
    }
    sem_mcp::run()
}
