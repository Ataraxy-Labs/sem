# Simple session policy (experimental)

Opt-in packaging of the September 24 PlantUML benchmark adapter. This does not
replace the default transaction server or its protocol-v2 revision contract.
It removes fixed discovery/edit call quotas and mandatory discovery-before-edit;
uses batched reads, acknowledged context reuse, scoped edits and coherent-batch
validation. Response size and snapshot-storage limits still apply.

## Run

Requires Node with TypeScript stripping (tested on Node 23.7), SEM 0.25.0 on PATH,
and the dependencies installed with `npm ci` in `sem/pi`. The benchmark used a
0.25.0 development build including the Python decorated-entity fix, not 0.22.1.

From the repository to edit:

```sh
sem graph --json > /dev/null
PI_SEM_MODE=transaction \
PI_SEM_CONFIG=/path/to/sem/pi/config/simple-transaction.mjs \
/path/to/sem/pi/node_modules/.bin/pi -e /path/to/sem/pi
```

For another MCP client, launch:

```sh
SEM_EXACT_TOOLS=1 node --experimental-strip-types /path/to/sem/pi/src/transaction/simple/sem-session-simple-mcp.mjs
```

Set its working directory to the target repository. The four tools are
`sem_plan`, `sem_exact`, `weave_transaction`, and `weave_program`. The companion
config contains the recommended agent instructions. Prewarming is explicit and
optional; subsequent index freshness is lazy, not an always-running daemon.

## Validation and limits

Local validation uses SEM's check API. The optional benchmark Docker checker
requires Python 3 (`SEM_PYTHON` overrides the executable) and all of
`SEM_VALIDATION_IMAGE`, `SEM_VALIDATION_CONTAINER`, `SEM_VALIDATION_CWD`, and
`SEM_VALIDATION_BASE`. Use a disposable image/container with the repository at
`/testbed` and dependencies already installed. Container cleanup belongs to the
caller. Do not use a production container or mount sensitive host resources.

Run only on trusted repositories in an appropriately sandboxed agent environment.
This server is not a security sandbox. Builds execute project code. It includes
text fallbacks, not exclusively graph-native operations. Exact edits check file
hashes and entity ranges inside Weave's process-local mutation queue; this is not
cross-process isolation. General transactions do not promise whole-repository
snapshot isolation. Rollback is compensating; concurrent writers need external
coordination. Parser coverage and passing tests do not prove semantic completeness.

## Evidence

One paired PlantUML SWE-Bench ProMax task (`plantuml__plantuml-2173`), same model
(gpt-6-astra, medium), both independently graded SUCCESS:

| | Native | Simple SEM+Weave |
|---|---:|---:|
| Session seconds | 398.759 | 287.919 |
| Total tokens including cached input | 1,931,171 | 1,854,702 |
| Separate index prewarm seconds | — | 1.776 |

27.8% shorter session, 4.0% fewer total tokens. Grading time is excluded. Including
index preparation gives 289.695 seconds. Noncached input was higher for SEM+Weave;
this is not a claim of lower billed cost. The solutions differed, and one pair
does not establish causality or consistent superiority. Cargo remained slightly
slower and Angular failed grading in the related trials. No universal speed claim.
The Pi config is an integration of the tested policy, not a new measured Pi trial.

## Tests

From `sem/pi`, with SEM 0.25.0 on PATH (or `SEM_TEST_BIN` set to its absolute path):

```sh
npm run test:simple
npm run typecheck
python3 -m unittest discover -s src/transaction/simple -p 'test_*.py'
```
