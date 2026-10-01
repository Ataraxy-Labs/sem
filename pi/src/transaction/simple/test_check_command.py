import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('check_command',Path(__file__).with_name('container-check-v8.py'))
checker=importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class CheckCommandTests(unittest.TestCase):
    def test_make_recipe_metacharacters_fail_before_execution(self):
        for value in ('-run TestA|TestB', 'x;echo bad', '$(echo bad)', '`echo bad`'):
            with self.subTest(value=value),self.assertRaisesRegex(ValueError,'direct test command'):
                checker.parse_check_command('make test GOTESTFLAGS='+__import__('shlex').quote(value))
        self.assertEqual(checker.parse_check_command('make test GO_TEST_PACKAGES="./a ./b"'), ['make','test','GO_TEST_PACKAGES=./a ./b'])

    def test_direct_regex_remains_single_argument(self):
        self.assertEqual(checker.parse_check_command("go test -run 'TestA|TestB' ./pkg"),['go','test','-run','TestA|TestB','./pkg'])

    def test_python_module_preserves_interpreter_and_test_selection(self):
        for interpreter in ('python','python3'):
            self.assertEqual(checker.parse_check_command(
                interpreter+' -m pytest tests/test_a.py -k "a or b" -q'),
                [interpreter,'-m','pytest','tests/test_a.py','-k','a or b','-q'])

    def test_entrypoint_unchanged(self):
        self.assertEqual(checker.parse_check_command('pytest -q tests'),['pytest','-q','tests'])

    def test_no_arbitrary_python_or_shell(self):
        for command in ('python -c "print(1)"','python script.py',
                        'python -m pip install pytest','python -m pytest_extra',
                        'python -I -m pytest','python3 -m pytest ; echo yes',
                        'pytest && echo yes','pytest | tee out',''):
            with self.subTest(command=command),self.assertRaises(ValueError):
                checker.parse_check_command(command)

    def test_invalid_quoting_is_unavailable_not_exception(self):
        result=checker._check('pytest "unterminated')
        self.assertIsNone(result['pass'])
        self.assertEqual(result['stage'],'unavailable')


if __name__=='__main__':
    unittest.main()
