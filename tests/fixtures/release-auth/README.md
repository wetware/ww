# Legacy installer compatibility fixture

`legacy-install.sh` is the unchanged installer from `origin/master` commit
`529a78fe366bcbd2d8b8f7f5a3c2d09001c8de99` before #412 authentication.
`tests/test_release_auth.py` runs it only against isolated fixture transport and
an executable test double. The fixture proves that the additional detached
signature preserves producer-first package compatibility. It is intentionally
unauthenticated and must never install real releases.
