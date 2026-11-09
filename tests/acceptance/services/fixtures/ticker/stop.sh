#!/bin/sh
docker rm -f "${1:-ticker}" >/dev/null 2>&1
exit 0
