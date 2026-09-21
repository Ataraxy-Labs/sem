# /// script
# requires-python = ">=3.11"
# dependencies = ["deepagents", "langchain-mcp-adapters", "mcp"]
# ///
"""Opt-in, real-process MCP checks. Run with uv; no model calls are made."""

import asyncio
import subprocess
import tempfile
from pathlib import Path

from deepagents_sem import TOOLS, sem_tools


async def check(root: Path, symbol: str) -> None:
    subprocess.run(["git", "init", "-q", str(root)], check=True)
    (root / "app.py").write_text(f"def {symbol}():\n    return 42\n")
    subprocess.run(["git", "-C", str(root), "add", "app.py"], check=True)
    async with sem_tools(str(root)) as tools:
        assert {tool.name for tool in tools} == TOOLS
        context = next(tool for tool in tools if tool.name == "sem_context")
        result = await context.ainvoke({"entity_name": symbol, "token_budget": 500})
        assert f"def {symbol}" in str(result), result
        refreshed = await context.ainvoke({"entity_name": symbol, "fresh": True})
        assert f"def {symbol}" in str(refreshed), refreshed
        print(f"PASS: allowlist, source retrieval, fresh context in {symbol}")


async def main() -> None:
    with (
        tempfile.TemporaryDirectory(prefix="sem-agent-a-") as first,
        tempfile.TemporaryDirectory(prefix="sem-agent-b-") as second,
    ):
        async with asyncio.timeout(60):
            await asyncio.gather(
                check(Path(first), "first_only"), check(Path(second), "second_only")
            )


if __name__ == "__main__":
    asyncio.run(main())
