#!/usr/bin/env python3
"""Action-schema contracts shared by the bundled research adapters."""

import unittest

from bash_agent import parse_action as parse_bash_action
from octocode_agent import parse_action as parse_octocode_action


class ActionSchemaTests(unittest.TestCase):
    def test_bash_accepts_only_complete_declared_actions(self):
        self.assertEqual(parse_bash_action('{"action":"shell","command":"grep -n x src/a.py"}')["action"], "shell")
        self.assertEqual(parse_bash_action('{"action":"page","step":1,"page":2}')["page"], 2)
        self.assertEqual(parse_bash_action('{"action":"finish","answer":"done"}')["answer"], "done")
        for invalid in [
            '{"action":"shell","command":""}',
            '{"action":"page","step":0,"page":1}',
            '{"action":"finish"}',
            '{"action":"octocode","tool":"localSearchCode","queries":{}}',
        ]:
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                parse_bash_action(invalid)

    def test_octocode_accepts_only_complete_declared_actions(self):
        action = parse_octocode_action(
            '{"action":"octocode","tool":"localSearchCode","queries":{"keywords":["router"]}}'
        )
        self.assertEqual(action["tool"], "localSearchCode")
        for invalid in [
            '{"action":"octocode","tool":"unknown","queries":{}}',
            '{"action":"octocode","tool":"localSearchCode"}',
            '{"action":"page","step":"1","page":2}',
            '{"action":"finish","answer":7}',
            '{"action":"shell","command":"ls"}',
        ]:
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                parse_octocode_action(invalid)


if __name__ == "__main__":
    unittest.main()
