#!/usr/bin/env python3
"""Run one test process group for at most 20 minutes, including cleanup on timeout."""
import os
import signal
import subprocess
import sys

if len(sys.argv) < 2:
    raise SystemExit("usage: bounded-test.py command [arguments...]")
process = subprocess.Popen(sys.argv[1:], start_new_session=True)
def interrupted(*unused):
    raise KeyboardInterrupt
signal.signal(signal.SIGTERM, interrupted)
try:
    # Leave room for process-group shutdown inside the 20-minute ceiling.
    raise SystemExit(process.wait(timeout=1190))
except (subprocess.TimeoutExpired, KeyboardInterrupt):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    raise SystemExit("test exceeded 20 minutes or was interrupted")
