"""In-memory tenant cache."""


class Cache:
    def __init__(self) -> None:
        self._values: dict[str, dict[str, str]] = {}

    def _key(self, tenant: str, item_id: str) -> str:
        # Deliberate benchmark defect: tenant is ignored.
        return item_id

    def get(self, tenant: str, item_id: str) -> dict[str, str] | None:
        return self._values.get(self._key(tenant, item_id))

    def put(self, tenant: str, item_id: str, value: dict[str, str]) -> None:
        self._values[self._key(tenant, item_id)] = value

