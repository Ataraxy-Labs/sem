# Sem + Weave agent skills

Two optional skills for agents that support portable Agent Plugins v1:

- **sem-code-review:** find functions/classes, inspect callers, and review entity-level Git diffs.
- **weave-merge-preview:** inspect a branch merge without applying it.

The package adds instructions, not model tools, hooks, a daemon, or a replacement
harness. It uses the host's existing terminal and approval flow. Sem and Weave
are independently useful; install Weave only if you want merge previews.

## Prerequisites

Install [Sem](https://github.com/Ataraxy-Labs/sem#install) and optionally
[Weave](https://github.com/Ataraxy-Labs/weave#install) yourself using their official
instructions. Tested with Sem **0.27.0** and Weave **0.5.4**, on macOS. Check
`sem --version`: GNU Parallel also provides an unrelated executable named `sem`.
The CLIs must be available in the environment where the host's terminal executes,
including inside a container if your agent uses one.

## Hermes

This directory is the plugin root. Choose a full reviewed commit SHA containing
this directory (from the source PR or catalog entry), then substitute it below:

```sh
hermes plugins install Ataraxy-Labs/sem/integrations/agent-plugin --ref FULL_40_CHARACTER_SHA --no-enable
hermes plugins list
```

Enable the installed name shown by `plugins list` with `hermes plugins enable NAME`
and start a new session. It is not yet included in the Hermes catalog. To remove
it, use `hermes plugins uninstall NAME`.

Try:

> In this repository, use sem-code-review to find parse_config and its callers.
> Summarize the uncommitted entity changes. Do not edit files or run tests.

Or, when the named branch exists:

> Use weave-merge-preview to preview feature-a against HEAD. Do not apply the merge.

Disable or uninstall the plugin with Hermes's plugin management commands. No Git
configuration is installed, and removing the plugin does not uninstall either CLI.

## Access and limitations

The package contains no executable payload or automatic installer. When invoked,
the skills ask the host to run local CLI processes that read the selected repository
and Git history. Sem may persist local indexes outside the repository. The Sem
commands disable cloud access and telemetry via per-command environment settings.
This does not sandbox the CLIs or prevent the host model provider from receiving
tool outputs. No credentials, usage reporting, background hooks, or self-updates
are added by this package.

The skills do not apply edits or merges. Static relationships can be incomplete,
and a clean merge preview is not compiler or behavioral verification. This package
does not claim an end-to-end speedup; benchmark the host and workload you use.

## Smoke tests

With both CLIs installed, run from the repository root:

```sh
python3 -m unittest discover -s integrations/agent-plugin/tests -v
```

Tests use disposable Git repositories, check real CLI output, and verify that
discovery/review/preview do not change HEAD, the index, or tracked source files.
They do not invoke a model or prove end-to-end agent behavior.
