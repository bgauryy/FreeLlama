import unittest
from agent_transport import resolve_model_identity


class IdentityTests(unittest.TestCase):
    def test_digest_and_prompt_changes_invalidate_identity(self):
        def resolve(digest, prompt="system"):
            return resolve_model_identity("http://local", "m:latest", prompt,
                read_json=lambda url: {"models":[{"name":"m:latest","digest":digest}]})
        first=resolve("a"*64)
        self.assertTrue(first["verified"])
        self.assertEqual(first["identity"],resolve("a"*64)["identity"])
        self.assertNotEqual(first["identity"],resolve("b"*64)["identity"])
        self.assertNotEqual(first["identity"],resolve("a"*64,"new system")["identity"])

    def test_managed_lookup_uses_operator_assigned_catalog_and_missing_digest_is_unknown(self):
        seen=[]
        def read(url):
            seen.append(url)
            return {"models":[{"name":"m:latest"}]}
        result=resolve_model_identity("http://local/_freellama/v1/tasks", "m", "system",read_json=read)
        self.assertEqual(seen,["http://local/_freellama/v1/models"])
        self.assertFalse(result["verified"])
        self.assertEqual(result["identity"],"")

    def test_unavailable_or_malformed_identity_never_enables_persistence(self):
        for payload in [[],{"models":None},{"models":[{"name":"m","digest":"wrong"}]}]:
            self.assertFalse(resolve_model_identity("http://local","m","secret system",read_json=lambda url:payload)["verified"])
        def failed(url):
            raise TimeoutError("unavailable")
        result=resolve_model_identity("http://local","m","secret system",read_json=failed)
        self.assertFalse(result["verified"])
        self.assertNotIn("secret",str(result))


if __name__=="__main__":
    unittest.main()
