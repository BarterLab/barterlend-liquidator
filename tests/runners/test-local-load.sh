#!/usr/bin/env bash
set -euo pipefail
task_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
task_cargo="${CARGO:-cargo}"
task_profile="${1:-all}"
task_output="${BARTERLEND_LOAD_OUTPUT:-$(mktemp -d "${TMPDIR:-/tmp}/barterlend-load.XXXXXX")}"
mkdir -p "$task_output"
snapshot_sources() {
  python3 - "$task_repo" "$1" "$task_cargo" "${BARTERLEND_RUST_TOOLCHAIN:-1.98.0}" <<'PY'
import hashlib,json,pathlib,subprocess,sys
repo=pathlib.Path(sys.argv[1]); files={}
cargo=[sys.argv[3], '+'+sys.argv[4]]
version=subprocess.check_output(cargo+['-vV'],text=True)
host=next((line.split(': ',1)[1] for line in version.splitlines() if line.startswith('host: ')),None)
if host is None: raise SystemExit('Cannot determine the native Cargo target for load evidence')
# The load process runs natively; other platforms must not require extra offline downloads.
metadata=json.loads(subprocess.check_output(cargo+['metadata', '--offline', '--locked',
    '--format-version', '1', '--filter-platform', host, '--manifest-path', str(repo/'Cargo.toml')],text=True))
packages=[package for package in metadata['packages'] if package['name']=='barterlend-state']
if len(packages)!=1 or not (packages[0].get('source') or '').startswith('git+'):
    raise SystemExit('Load evidence requires exactly one Git-sourced state SDK dependency')
sdk=packages[0]
for label,root in (('liquidator',repo),('state',pathlib.Path(sdk['manifest_path']).parent)):
    paths=list((root/'src').rglob('*.rs'))+list((root/'tests/unit').rglob('*.rs'))+list((root/'abi').glob('*.json'))
    paths += [root/'Cargo.toml',root/'Cargo.lock']
    if root==repo: paths.append(root/'examples/local-manifest.json')
    for path in sorted(paths):
        if path.is_file(): files[label+'/'+str(path.relative_to(root))]=hashlib.sha256(path.read_bytes()).hexdigest()
digest=hashlib.sha256(json.dumps(files,sort_keys=True,separators=(',',':')).encode()).hexdigest()
pathlib.Path(sys.argv[2]).write_text(json.dumps({'sha256':digest,'sdk_source':sdk['source'],'files':files},indent=2)+'\n')
PY
}
case "$task_profile" in
  all) task_profiles=(1200 4000) ;;
  1200|4000) task_profiles=("$task_profile") ;;
  *) echo "usage: $0 [all|1200|4000]" >&2; exit 2 ;;
esac
for task_case in "${task_profiles[@]}"; do
  task_rps=15;task_positions="$task_case"
  if [[ "$task_case" == 1200 ]]; then task_pools=4;else task_pools=8;fi
  echo "Local HTTP load: $task_case; report $task_output/$task_case.json"
  snapshot_sources "$task_output/$task_case.sources-before.json"
  task_exit=0
  BARTERLEND_LOAD_POOLS="$task_pools" BARTERLEND_LOAD_POSITIONS="$task_positions" \
    BARTERLEND_LOAD_RPS="$task_rps" BARTERLEND_LOAD_REPORT="$task_output/$task_case.json" \
    python3 "$task_repo/scripts/bounded-test.py" "$task_cargo" +"${BARTERLEND_RUST_TOOLCHAIN:-1.98.0}" test --offline --locked --manifest-path "$task_repo/Cargo.toml" --lib \
    app::load_tests::http_300ms_many_pools_positions -- --ignored --exact --nocapture \
    > "$task_output/$task_case.log" 2>&1 || task_exit=$?
  snapshot_sources "$task_output/$task_case.sources-after.json"
  python3 - "$task_output" "$task_case" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1]); name=sys.argv[2]
before=json.loads((root/(name+'.sources-before.json')).read_text())
after=json.loads((root/(name+'.sources-after.json')).read_text())
path=root/(name+'.json')
if path.exists():
    result=json.loads(path.read_text()); result['source_fingerprint']=before['sha256']
    result['sources_unchanged_during_run']=before==after
    if before!=after: result['passed']=False
    path.write_text(json.dumps(result,indent=2)+'\n')
if before!=after: raise SystemExit('Sources changed during load run; evidence is preliminary, rerun required')
PY
  if [[ "$task_exit" != 0 ]]; then
    python3 - "$task_output/$task_case.log" <<'PY'
import pathlib,sys
lines=pathlib.Path(sys.argv[1]).read_text().splitlines()
print('\n'.join(line for line in lines if line.startswith(('load progress','LOAD_RESULT','error:','thread ','test result:'))))
PY
    exit "$task_exit"
  fi
  tail -n 6 "$task_output/$task_case.log"
done
echo "Load evidence: $task_output"
