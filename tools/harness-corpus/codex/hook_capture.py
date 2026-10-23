#!/usr/bin/env python3
"""Capture isolated live hook inputs and cv outputs; never used against a real harness home."""
import json
import os
import subprocess
import sys

raw = sys.stdin.read()
result = subprocess.run(sys.argv[2:], input=raw, text=True, capture_output=True)
row = {"input": json.loads(raw), "stdout": result.stdout, "stderr": result.stderr,
       "exit_code": result.returncode}
with open(sys.argv[1], "a", encoding="utf-8") as output:
    output.write(json.dumps(row) + "\n")
sys.stdout.write(result.stdout)
sys.stderr.write(result.stderr)
sys.exit(result.returncode)
