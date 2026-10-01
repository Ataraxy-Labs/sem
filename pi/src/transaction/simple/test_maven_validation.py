import unittest
from validation_resources import bounded_command

class MavenValidation(unittest.TestCase):
    def test_preserves_targets_and_forces_offline(self):
        args=['mvn','-pl','hbase-server','-Dtest=TestExample','test']
        self.assertEqual(bounded_command(args),['mvn','--offline',*args[1:]])
        self.assertEqual(args[1],'-pl')

    def test_offline_is_not_duplicated(self):
        for args in [['mvn','-o','test'],['./mvnw','--offline','verify']]:
            self.assertEqual(bounded_command(args),args)
