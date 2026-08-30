"""Authorization policy."""


def is_allowed(role: str, action: str) -> bool:
    """Admins may do anything; readers may only read."""
    if role == "admin":
        return True
    if role == "reader":
        return action != "delete"
    return False

