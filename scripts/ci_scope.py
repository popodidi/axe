#!/usr/bin/env python3
"""Decide whether a pull request's CI run needs the web job.

`scope` (ci.yml) writes `ui=true|false` to $GITHUB_OUTPUT (stdout without it).

A pull request sets `ui` when `web/` or the dashboard-serving Rust code changed
(`web.rs`, `tests/embed_ui.rs`, or a Rust file mentioning
`feature = "embed-ui"` before or after the change). Changes to the build or to
CI (`Cargo.toml`, `Cargo.lock`, `build.rs`, `rust-toolchain*`, `.github/`,
`scripts/ci_*`) set it too. Any other event sets it: the release gate
(ci.yml with `full: true`) runs everything without asking.
"""

import os
import re
import subprocess
import sys

EMBED_UI_CFG = re.compile(r'feature\s*=\s*"embed-ui"')
UI_RUST = {'crates/axe/src/web.rs', 'crates/axe/tests/embed_ui.rs'}


def git(*args):
    return subprocess.run(('git',) + args, check=True, capture_output=True, text=True).stdout


def blob(rev, path):
    """The file at `rev`, or '' when it does not exist there."""
    done = subprocess.run(('git', 'show', f'{rev}:{path}'), capture_output=True, text=True)
    return done.stdout if done.returncode == 0 else ''


def runs_everything(path):
    name = path.rsplit('/', 1)[-1]
    return (name in ('Cargo.toml', 'Cargo.lock', 'build.rs') or name.startswith('rust-toolchain')
            or path.startswith('.github/') or path.startswith('scripts/ci_'))


def needs_ui(paths, versions):
    """Whether a pull request changing `paths` needs the web job; `versions(path)` yields its texts."""
    for path in paths:
        if runs_everything(path) or path.startswith('web/') or path in UI_RUST:
            return True
        if path.endswith('.rs') and any(EMBED_UI_CFG.search(t) for t in versions(path)):
            return True
    return False


def pull_request_ui():
    # The pull_request checkout is the merge commit; its first parent is the base.
    base = git('rev-parse', 'HEAD^1').strip()
    paths = git('diff', '--name-only', '--no-renames', base, 'HEAD').split()
    print(f'{len(paths)} changed file(s) against {base[:12]}:')
    for path in paths:
        print(f'  {path}')
    return needs_ui(paths, lambda path: (blob(base, path), blob('HEAD', path)))


def scope():
    ui = pull_request_ui() if os.environ.get('GITHUB_EVENT_NAME') == 'pull_request' else True
    line = f'ui={str(ui).lower()}\n'
    print(line, end='')
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a') as out:
            out.write(line)
    return 0


def main(argv):
    if argv[1:] == ['scope']:
        return scope()
    print(f'usage: {argv[0]} scope', file=sys.stderr)
    return 2


if __name__ == '__main__':
    sys.exit(main(sys.argv))
