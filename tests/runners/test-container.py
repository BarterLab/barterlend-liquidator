#!/usr/bin/env python3
"""Exercise the built image offline, without any wallet or public RPC."""
import argparse
import json
import pathlib
import re
import subprocess
import tempfile
import time
import uuid


def docker(*args, check=True, timeout=40):
    result = subprocess.run(['docker', *args], text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError('docker ' + args[0] + ' failed: ' + result.stderr.strip())
    return result


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def probe(container, path):
    # Debian includes Bash/coreutils; no extra HTTP client is needed in the runtime image.
    script = 'exec 3<>/dev/tcp/127.0.0.1/9090; printf "GET %s HTTP/1.1\r\nHost: localhost\r\n\r\n" "$1" >&3; cat <&3'
    return docker('exec', container, 'timeout', '3', '/bin/bash', '-ec', script, 'probe', path, check=False).stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('image')
    args = parser.parse_args()
    require(not args.image.startswith('-'), 'An image reference is required')
    flags = ['--network', 'none', '--read-only', '--cap-drop', 'ALL',
             '--security-opt', 'no-new-privileges', '--ulimit', 'core=0']
    config = json.loads(docker('inspect', '--format', '{{json .Config}}', args.image).stdout)
    require(config['User'] == '10001:10001', 'Runtime UID/GID must be 10001')
    require(config['Entrypoint'] == ['/usr/local/bin/barterlend-liquidator'], 'Unexpected entrypoint')
    require(not any(value.startswith(('BL_', 'GITHUB_TOKEN=')) for value in config.get('Env', [])),
            'Runtime credentials/config must not be baked into the image')
    docker('run', '--rm', *flags, args.image, '--help')
    docker('run', '--rm', *flags, '--entrypoint', '/bin/sh', args.image, '-ec',
           'test ! -e /config/config.toml; test ! -e /keys/operator.key; test ! -e /build; test ! -e /root/.cargo; command -v install; command -v bash')
    docker('run', '--rm', *flags, '--tmpfs', '/keys:rw,noexec,nosuid,nodev,size=1m,uid=10001,gid=10001,mode=0700',
           '--entrypoint', '/bin/sh', args.image, '-ec',
           'printf fixture-only > /keys/projected; chmod 440 /keys/projected; '
           'install -m 600 /keys/projected /keys/operator.key; '
           'test "$(stat -c %a:%u:%h /keys/operator.key)" = 600:10001:1')
    with tempfile.TemporaryDirectory(prefix='liquidator-image-test-') as directory:
        container = 'liquidator-image-check-' + uuid.uuid4().hex
        try:
            root = pathlib.Path(__file__).resolve().parents[2]
            manifest = json.loads((root / 'examples/local-manifest.json').read_text())
            sentinel = 'CONTAINER_RPC_REDACTION_SENTINEL'
            manifest['rpc_url'] = 'http://127.0.0.1:1/' + sentinel
            manifest['health_listen'] = '0.0.0.0:9090'
            manifest['rpc_fallback_urls'] = []
            path = pathlib.Path(directory) / 'config.json'
            path.write_text(json.dumps(manifest))
            path.chmod(0o644)
            docker('run', '-d', '--name', container, *flags, '--mount',
                'type=bind,source=' + str(path) + ',target=/config.json,readonly',
                args.image, '--config', '/config.json')
            deadline = time.monotonic() + 40
            while not probe(container, '/live').startswith('HTTP/1.1 200 '):
                require(time.monotonic() < deadline, 'Liveness did not start without RPC')
                state = json.loads(docker('inspect', '--format', '{{json .State}}', container).stdout)
                require(state['Running'], 'Runtime stopped during startup')
                time.sleep(0.25)
            require(probe(container, '/ready').startswith('HTTP/1.1 503 '), 'Unavailable RPC must not report ready')
            metrics = probe(container, '/metrics')
            require(metrics.startswith('HTTP/1.1 200 ') and 'barterlend_ready 0' in metrics, 'Metrics unavailable')
            docker('stop', '--time', '25', container)
            state = json.loads(docker('inspect', '--format', '{{json .State}}', container).stdout)
            require(state['ExitCode'] == 0 and not state['OOMKilled'], 'SIGTERM shutdown failed')
            logs = docker('logs', container)
            require(not logs.stderr, 'Application logs must use stdout')
            require(sentinel not in logs.stdout and '\x1b' not in logs.stdout, 'Logs contain sensitive input or ANSI escapes')
            require('stopped' in logs.stdout, 'Missing shutdown event')
            for line in logs.stdout.splitlines():
                require(re.match(r'^\d{4}-\d\d-\d\dT\S+Z\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+', line) is not None,
                        'Unexpected console log envelope')
        finally:
            # Remove even a partially started container before removing its mounted fixture.
            docker('rm', '-f', container, check=False)
    print('Image PASS: non-root/read-only, key-file copy permissions, startup without RPC, probes, stdout/redaction and SIGTERM; network disabled.')


if __name__ == '__main__':
    main()
