#!/usr/bin/env python3
"""Verify that the release uses exactly the reviewed Git SDK, without local overrides."""
import pathlib
import re
try:
    import tomllib
except ImportError:
    raise SystemExit("Release checks require Python 3.11 or newer (tomllib).")


def check(root):
    manifest = tomllib.loads((root / 'Cargo.toml').read_text())
    dependency = manifest['dependencies']['barterlend-state']
    if not isinstance(dependency, dict) or 'path' in dependency or not re.fullmatch('[0-9a-f]{40}', dependency.get('rev', '')):
        raise ValueError('Pin the published SDK by its full Git SHA, without a path dependency.')
    url = 'https://github.com/BarterLab/barterlend-state.git'
    if dependency.get('git') != url:
        raise ValueError('Review the SDK Git source; update this gate for an intentional fork.')
    if manifest.get('patch') or manifest.get('replace'):
        raise ValueError('Review dependency overrides before release; patches/replacements are not allowed by this gate.')
    lock = tomllib.loads((root / 'Cargo.lock').read_text())
    packages = [p for p in lock['package'] if p['name'] == 'barterlend-state']
    expected = 'git+' + url + '?rev=' + dependency['rev'] + '#' + dependency['rev']
    if len(packages) != 1 or packages[0].get('source') != expected:
        raise ValueError('Cargo.lock must contain exactly the pinned SDK Git source and revision.')


if __name__ == '__main__':
    try:
        check(pathlib.Path(__file__).resolve().parent.parent)
    except (KeyError, ValueError) as error:
        raise SystemExit(str(error))
    print('SDK release pin and lock agree.')
