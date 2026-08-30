"""Public request entry point."""

from .service import Service


def handle_request(tenant: str, item_id: str) -> dict[str, str]:
    """Resolve one item for a tenant."""
    return Service().execute(tenant, item_id)

