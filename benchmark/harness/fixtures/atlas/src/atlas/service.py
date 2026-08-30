"""Application service."""

from .cache import Cache
from .store import Store


class Service:
    def __init__(self, store: Store | None = None, cache: Cache | None = None) -> None:
        self.store = store or Store()
        self.cache = cache or Cache()

    def execute(self, tenant: str, item_id: str) -> dict[str, str]:
        cached = self.cache.get(tenant, item_id)
        if cached is not None:
            return cached
        value = self.store.fetch(tenant, item_id)
        self.cache.put(tenant, item_id, value)
        return value

