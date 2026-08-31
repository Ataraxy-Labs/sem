#!/usr/bin/env node
// Fake `sem` binary stub for the exit-0-with-human-text regression case:
// a teammate independently reproduced this repo's ambiguous-entity bug
// against a DIFFERENT sem build that signals the refusal via exit 0 with
// the human-readable candidate list on STDOUT, rather than the exit-1 +
// STDERR shape this machine's installed sem 0.23.1 actually produces
// (confirmed by direct reproduction against real sem, see runSemJson's
// doc comment in api.ts). Since which shape a given `sem` build uses is
// downstream-version-dependent and out of api.ts's control, runSemJson
// has to handle both -- this stub exists to prove the exit-0 path
// specifically, deterministically, without depending on any particular
// installed sem version behaving one way or the other.
process.stdout.write("error: Entity 'widget' found in multiple files:\n  a.ts\n  b.ts\n\nUse --file to disambiguate.\n");
process.exit(0);
