// Serialize explicit argv for the checker's shlex parser, not a shell.
export function normalizeValidation(params) {
  if (!Object.hasOwn(params, 'validation_argv')) return params;
  if (Object.hasOwn(params, 'validation_cmd')) throw new Error('Use validation_argv OR validation_cmd, not both');
  const argv = params.validation_argv;
  if (!Array.isArray(argv) || !argv.length || argv.length>128 || argv.some(x=>typeof x!=='string'||/[\0\r\n]/.test(x)))
    throw new Error('validation_argv must contain 1–128 string arguments without NUL or newlines');
  const {validation_argv, ...rest} = params;
  return {...rest, validation_cmd: argv.map(x=>"'"+x.replaceAll("'", "'\"'\"'")+"'").join(' ')};
}
