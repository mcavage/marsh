#!/usr/bin/env python3
"""Compatibility entry point for the focused acceptance smoke gate."""

from run import smoke_main


if __name__ == "__main__":
    raise SystemExit(smoke_main())
