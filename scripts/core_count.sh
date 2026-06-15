#!/usr/bin/env bash
if n=$(sysctl -n hw.perflevel0.physicalcpu 2>/dev/null) && [ -n "$n" ]; then
    echo "$n"
elif n=$(sysctl -n hw.physicalcpu 2>/dev/null) && [ -n "$n" ]; then
    echo "$n"
elif command -v nproc >/dev/null 2>&1; then
    nproc
else
    echo 1
fi
