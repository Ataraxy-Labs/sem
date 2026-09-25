"""Pinned resource policy for both arms; never changes test selection."""
BAZEL_FLAGS = ['--jobs=2', '--local_cpu_resources=2', '--local_ram_resources=4096',
               '--worker_max_instances=1', '--experimental_worker_max_multiplex_instances=1',
               '--local_test_jobs=1', '--worker_quit_after_build']

def bounded_command(argv):
    argv = list(argv)
    start = 0
    if argv[:1] in (['yarn'], ['npm'], ['pnpm']):
        start = 2 if argv[1:2] == ['run'] else 1
    if argv[start:start+1] not in (['bazel'], ['bazelisk']):
        return argv
    verb = next((i for i in range(start+1, len(argv))
                 if not argv[i].startswith('-')), None)
    if verb is None or argv[verb] not in ('test', 'build'):
        return argv
    keys = {f.split('=')[0] for f in BAZEL_FLAGS} | {'--host_jvm_args'}
    if any(a.split('=')[0] in keys for a in argv[start+1:]):
        raise ValueError('Build resources are runner-managed identically for both arms')
    return (argv[:start+1] + ['--host_jvm_args=-Xmx1024m'] + argv[start+1:verb+1]
            + BAZEL_FLAGS + argv[verb+1:])
