# Streamable HTTP MCP (preview)

Use HTTP when a trusted agent supports URL-based MCP but cannot launch stdio
servers. Stdio remains the default. The preview ships in sem 0.28.0 and later;
older published binaries do not include it.

Build from this checkout:

```sh
cargo build --manifest-path crates/Cargo.toml -p sem-cli
```

Start one endpoint for one trusted Git checkout:

```sh
export SEM_REPO=/absolute/path/to/your/repository
export SEM_MCP_HTTP_TOKEN="$(openssl rand -hex 32)"
export SEM_NO_NETWORK=1 SEM_NO_TELEMETRY=1
./crates/target/debug/sem mcp --http 127.0.0.1:8765
```

In your client's Streamable HTTP configuration, set the URL to
`http://127.0.0.1:8765/mcp` and the `Authorization` header to `Bearer <your token>`.
Use the numeric address exactly, not `localhost`. Keep the token in the client's
secret store, not in a committed config. Restarting with a new token revokes the old
one. Stop the process to remove access. No client configuration is changed for you.

The endpoint exposes only `sem_find` and `sem_grep`. For example:

```json
{"name":"sem_find","arguments":{"query":"parse_config","mode":"context","token_budget":1200}}
```

Both transports use the same Sem implementation. HTTP shares repository caches,
but is stateless: read-history deduplication does not persist across requests.
Index/cache files may be written; “read-only” means no source edits, builds, or
repository validation commands. Weave edits and merges are **not** exposed here.

## Security and deployment boundary

- Loopback only, mandatory bearer token, exact Host validation, browser Origin
  requests rejected, no CORS. It is not an OAuth discovery endpoint.
- Only two tools are advertised **and callable**. Hidden tools and cloud review
  methods are not forwarded. Requests are limited to 1 MiB and eight concurrent
  HTTP requests; this is not a guarantee of bounded indexing CPU or memory.
- Paths must be repository-relative. Traversal and existing symlink escapes in
  explicit paths are rejected. **This is not a filesystem sandbox**: indexes and
  subprocesses run with the server user's permissions. Do not serve untrusted
  checkouts or share the token with untrusted clients. Source files can contain
  secrets; anyone holding the token is trusted to read this checkout.
- A remote client needs a trusted SSH tunnel to this listener; preserve the
  endpoint's Host authority. A hosted client cannot read your laptop merely by
  receiving this URL. `127.0.0.1` refers to the machine running the MCP client.
- Public hosting, browser clients, OAuth, tenant isolation, and remote execution
  require additional design. Do not expose this preview directly to the internet
  or disable its protections to make a hosted integration connect.

For ordinary local agents, prefer `sem mcp` over stdio.
