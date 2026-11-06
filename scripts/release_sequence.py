#!/usr/bin/env python3
"""Encode workflow order and attempt order as a positive signed release sequence."""
import sys


def release_sequence(run_number, run_attempt):
    if not (0 < run_number < (1 << 47) and 0 < run_attempt < (1 << 16)):
        raise ValueError("release workflow counter exceeds the signed sequence profile")
    return (run_number << 16) | run_attempt


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3:
            raise ValueError("usage: release_sequence.py RUN_NUMBER RUN_ATTEMPT")
        print(release_sequence(int(sys.argv[1]), int(sys.argv[2])))
    except ValueError as error:
        sys.exit(str(error))
