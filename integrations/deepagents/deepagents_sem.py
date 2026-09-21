# /// script
# requires-python = ">=3.11"
# dependencies = ["deepagents", "langchain-mcp-adapters", "mcp"]
# ///
"""Load Sem into Deep Agents using a persistent, checkout-local MCP session."""

import argparse
import asyncio
import json
import os
import shutil
import subprocess
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from pathlib import Path

from langchain_core.tools import BaseTool
from langchain_mcp_adapters.tools import load_mcp_tools
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

TOOLS = {"sem_entities", "sem_context", "sem_impact", "sem_diff"}


@asynccontextmanager
async def sem_tools(
    repo: str, executable: str = "sem"
) -> AsyncIterator[list[BaseTool]]:
    """Keep one session alive across agent turns, preserving context deduplication.

    Run inside the agent's checkout/container, not against a host-side mirror.
    Tool filtering is a model-facing policy, not a filesystem security boundary.
    """
    binary = shutil.which(executable)
    if binary is None:
        raise ValueError(f"Sem executable not found: {executable}")
    root = subprocess.run(
        ["git", "-C", str(Path(repo).resolve()), "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=True,
        timeout=10,
    ).stdout.strip()
    # Do not inherit credentials into the local analysis subprocess.
    env = {
        key: os.environ[key]
        for key in ("PATH", "HOME", "SYSTEMROOT", "TMPDIR")
        if key in os.environ
    }
    env.update(SEM_CLOUD="0", SEM_MCP_CLOUD="0")
    params = StdioServerParameters(command=binary, args=["mcp"], cwd=root, env=env)
    async with stdio_client(params) as (reader, writer):
        async with ClientSession(reader, writer) as session:
            await session.initialize()
            tools = [
                tool for tool in await load_mcp_tools(session) if tool.name in TOOLS
            ]
            missing = TOOLS - {tool.name for tool in tools}
            if missing:
                raise RuntimeError(f"Sem is missing required tools: {sorted(missing)}")
            yield tools


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--sem", default="sem")
    parser.add_argument("--entity", help="Smoke-test context retrieval without an LLM")
    args = parser.parse_args()
    async with sem_tools(args.repo, args.sem) as tools:
        if args.entity:
            context = next(tool for tool in tools if tool.name == "sem_context")
            print(
                await context.ainvoke(
                    {"entity_name": args.entity, "token_budget": 1500}
                )
            )
        else:
            print(json.dumps({"tools": [tool.name for tool in tools]}))


if __name__ == "__main__":
    asyncio.run(main())
