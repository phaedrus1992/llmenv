#!/usr/bin/env bash
if command -v nproc >/dev/null 2>&1; then
    nproc
elif n=$(sysctl -n hw.physicalcpu 2>/dev/null) && [ -n "$n" ]; then
    echo "$n"
else
    echo 1
fi
