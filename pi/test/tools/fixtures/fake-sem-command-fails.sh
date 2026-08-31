#!/bin/sh
# A fake `sem` binary that always fails realistically: empty stdout, a real
# human-readable error on stderr, non-zero exit -- the exact shape observed
# empirically against a real sem 0.23.1 binary for several distinct failure
# causes (an unrecognized flag: "error: unexpected argument '--foo' found
# ... Usage: sem graph ...", exit 2; a corrupted/unreadable index; a crash).
# Used to prove, deterministically (no real sem failure needs to be
# engineered), whether a given TypeScript call site actually surfaces this
# text to its caller or silently discards it behind a generic
# "invalid JSON" parse-failure message.
echo "error: unexpected argument '--this-flag-does-not-exist' found

  tip: to pass '--this-flag-does-not-exist' as a value, use '-- --this-flag-does-not-exist'

Usage: sem graph --json --no-default-excludes [PATH]

For more information, try '--help'." >&2
exit 2
