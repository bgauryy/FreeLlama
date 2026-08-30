"""Catalog lookup with a deliberately inefficient representation."""

ITEMS = [f"item-{index}" for index in range(20_000)]


def contains(item_id: str) -> bool:
    return item_id in ITEMS

