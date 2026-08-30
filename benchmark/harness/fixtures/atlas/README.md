# Atlas fixture

Atlas is a deliberately small but cross-file service used by the FreeLlama agent benchmark. Treat `src/atlas/api.py` as the HTTP entry point and `src/atlas/cli.py` as the command-line entry point. Run regression tests with `PYTHONPATH=src python3 -m unittest discover -s tests`.

