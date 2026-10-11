---
name: weave-merge-preview
description: Preview how a named branch would merge into HEAD using the locally installed Weave CLI, without applying the merge. Use when reviewing overlapping branch or parallel-agent changes.
---

# Preview a branch merge

Use the host's terminal in the user's chosen Git repository. Confirm `weave --version`
is Ataraxy Weave (tested with 0.5.4). If absent, explain the prerequisite;
do not install it or alter Git's merge driver automatically.

Identify the exact branch/ref the user wants compared with the current HEAD.
Check the current branch and working-tree status using the host's Git tools.
Use `weave preview BRANCH`, replacing BRANCH with the shell-quoted requested ref.
To narrow a known conflict, `weave preview BRANCH --file path/to/file` previews
that file. Do not fetch remote branches unless the task authorizes it.

Report the conflicts or independent entity changes the preview actually shows.
A clean structural merge is not proof that the combined program compiles or
behaves correctly. Unsupported file types may use line-level merge behavior.

This workflow previews only. Do not run `weave setup`, `weave land`, `git merge`,
stage changes, or modify source because a preview succeeded. Applying a merge
and running the repository's build/tests require the user's corresponding request
and the host's normal approval flow.
