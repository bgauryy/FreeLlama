"""Persistence boundary."""


class Store:
    def fetch(self, tenant: str, item_id: str) -> dict[str, str]:
        return {"tenant": tenant, "item_id": item_id, "value": f"{tenant}:{item_id}"}

