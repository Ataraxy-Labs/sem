# Local structural code context with Sem

This example loads four tools from [Sem](https://github.com/Ataraxy-Labs/sem)
using the standard LangChain MCP adapter. Install Sem separately and make sure
`sem mcp --help` works. Run from the same machine/container as the agent's actual
Git checkout, not from a host-side copy of a remote sandbox.

```sh
uv run deepagents_sem.py --repo /absolute/path/to/checkout
uv run deepagents_sem.py --repo /absolute/path/to/checkout --entity SomeFunction
```

These commands test discovery and retrieval without invoking a model. To use
the tools in a Deep Agent application, install `deepagents`,
`langchain-mcp-adapters`, and `mcp`, then:

```python
from deepagents import create_deep_agent
from deepagents_sem import sem_tools

async def investigate(model, repo, question):
    async with sem_tools(repo) as tools:
        agent = create_deep_agent(model=model, tools=tools)
        return await agent.ainvoke({
            "messages": [{"role": "user", "content": question}]
        })
```

Pass your application's configured model. The session must stay open throughout
the conversation, because tool calls use it and Sem can deduplicate repeated
context. Request `fresh=True` after conversation compaction when earlier source
context is no longer available.

The allowlist exposes `sem_entities`, `sem_context`, `sem_impact`, and `sem_diff`.
Native tools remain available for edits, tests, text search, and incomplete
structural coverage. This is not a sandbox or an atomic editing transaction.
Cloud graph access is disabled; Sem may write local cache files. No performance
or correctness improvement is claimed without paired end-to-end measurements.
