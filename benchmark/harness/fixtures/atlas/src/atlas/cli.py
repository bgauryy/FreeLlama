"""Command-line entry point."""

import json
import sys

from .api import handle_request


def main(argv: list[str] | None = None) -> int:
    args = argv if argv is not None else sys.argv[1:]
    if len(args) != 2:
        return 2
    print(json.dumps(handle_request(args[0], args[1]), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

