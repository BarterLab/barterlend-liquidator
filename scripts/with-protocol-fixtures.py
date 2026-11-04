#!/usr/bin/env python3
"""Temporarily attach private protocol sources for integration builds, never for release images."""
import argparse
import os
import pathlib
import subprocess
import sys
import signal

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--source',default=os.environ.get('BARTERLEND_PROTOCOL_FIXTURES'))
parser.add_argument('command',nargs=argparse.REMAINDER)
args=parser.parse_args()
if not args.source or not args.command:parser.error('--source (or BARTERLEND_PROTOCOL_FIXTURES) and a command are required')
root=pathlib.Path(__file__).resolve().parent.parent
source=pathlib.Path(args.source).resolve(strict=True)
if source==root or root in source.parents:parser.error('private fixture source must be outside this repository')
links=[(root/'tests/fixtures/solidity/protocol-v5'/name,source/'protocol-v5'/name) for name in ['src','test','foundry.toml']]
links.append((root/'tests/fixtures/upstream-compat',source/'upstream-compat'))
for target,origin in links:
    if not origin.exists():raise SystemExit('Private fixture input missing: '+str(origin))
    if target.exists() or target.is_symlink():raise SystemExit('Fixture mount already exists; refuse concurrent or destructive replacement: '+str(target))
attached=[]
process=None
def interrupted(*unused):
    raise KeyboardInterrupt
signal.signal(signal.SIGTERM,interrupted)
try:
    for target,origin in links:
        target.parent.mkdir(parents=True,exist_ok=True)
        target.symlink_to(origin,target_is_directory=origin.is_dir());attached.append((target,origin))
    command=args.command[1:] if args.command[0]=='--' else args.command
    subprocess.run([sys.executable,str(root/'scripts/verify-solidity-fixtures.py'),'--inputs-only'],check=True)
    process=subprocess.Popen(command,cwd=root,start_new_session=True)
    raise SystemExit(process.wait())
finally:
    if process is not None and process.poll() is None:
        os.killpg(process.pid,signal.SIGTERM)
        try:process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid,signal.SIGKILL);process.wait()
    for target,origin in reversed(attached):
        if target.is_symlink() and target.resolve()==origin:target.unlink()
