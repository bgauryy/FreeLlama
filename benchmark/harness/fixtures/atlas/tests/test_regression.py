import unittest

from atlas.auth import is_allowed
from atlas.config import timeout_seconds
from atlas.service import Service


class RegressionTests(unittest.TestCase):
    def test_service_returns_requested_item(self) -> None:
        self.assertEqual(Service().execute("a", "1")["value"], "a:1")

    def test_unknown_role_is_denied(self) -> None:
        self.assertFalse(is_allowed("unknown", "read"))

    def test_default_timeout(self) -> None:
        self.assertEqual(timeout_seconds({}), 30)


if __name__ == "__main__":
    unittest.main()

