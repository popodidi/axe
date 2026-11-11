#!/usr/bin/env python3
"""Decide which CI steps a run needs, and whether a commit passed CI on main.

`scope` (ci.yml) writes `rust`, `telegram` and `ui` as true/false lines to
$GITHUB_OUTPUT (stdout without it):

- rust: default-feature fmt, clippy and tests, and the TS types test.
- telegram: the no-default-features clippy and tests.
- ui: the embed-ui build and test, and the web job.

A pull request runs `rust` always; `telegram` when a changed Rust file
mentions `feature = "telegram"` (before or after the change) or is in the
Telegram module; `ui` when `web/` or the dashboard-serving Rust code changed
(`web.rs`, `tests/embed_ui.rs`, or a Rust file mentioning
`feature = "embed-ui"`). Changes to the build or to CI (`Cargo.toml`,
`Cargo.lock`, `build.rs`, `rust-toolchain*`, `.github/`, `scripts/ci_*`) run
everything. Pushes to main run everything. A `v*` tag push runs nothing when
its commit already passed CI on main, and everything otherwise.

`main-ci <sha>` (release.yml) exits 0 when a push to main ran ci.yml green on
<sha>, and 1 otherwise.
"""

import json
import os
import re
import subprocess
import sys
import urllib.request

FLAGS = ('rust', 'telegram', 'ui')
TELEGRAM_CFG = re.compile(r'feature\s*=\s*"telegram"')
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


def telegram_module(path):
    return path.startswith('crates/') and any(part.startswith('telegram') for part in path.split('/'))


def select(paths, versions):
    """Flags for a pull request changing `paths`; `versions(path)` yields its texts."""
    flags = dict.fromkeys(FLAGS, False)
    flags['rust'] = True
    for path in paths:
        if runs_everything(path):
            return dict.fromkeys(FLAGS, True)
        if path.startswith('web/') or path in UI_RUST:
            flags['ui'] = True
        if not path.endswith('.rs'):
            continue
        if telegram_module(path):
            flags['telegram'] = True
        texts = versions(path)
        if any(TELEGRAM_CFG.search(t) for t in texts):
            flags['telegram'] = True
        if any(EMBED_UI_CFG.search(t) for t in texts):
            flags['ui'] = True
    return flags


def pull_request_flags():
    # The pull_request checkout is the merge commit; its first parent is the base.
    base = git('rev-parse', 'HEAD^1').strip()
    paths = git('diff', '--name-only', '--no-renames', base, 'HEAD').split()
    print(f'{len(paths)} changed file(s) against {base[:12]}:')
    for path in paths:
        print(f'  {path}')
    return select(paths, lambda path: (blob(base, path), blob('HEAD', path)))


def main_ci_green(sha):
    """True when a push to main ran ci.yml successfully on `sha`."""
    repo = os.environ['GITHUB_REPOSITORY']
    api = os.environ.get('GITHUB_API_URL', 'https://api.github.com')
    url = (f'{api}/repos/{repo}/actions/workflows/ci.yml/runs'
           f'?head_sha={sha}&branch=main&event=push&status=success&per_page=1')
    request = urllib.request.Request(url, headers={
        'Accept': 'application/vnd.github+json',
        'Authorization': f'Bearer {os.environ["GITHUB_TOKEN"]}',
        'X-GitHub-Api-Version': '2022-11-28',
    })
    with urllib.request.urlopen(request, timeout=30) as response:
        runs = json.load(response)['workflow_runs']
    return any(r['head_sha'] == sha and r['head_branch'] == 'main' for r in runs)


def scope():
    event = os.environ.get('GITHUB_EVENT_NAME', '')
    ref = os.environ.get('GITHUB_REF', '')
    if event == 'pull_request':
        flags = pull_request_flags()
    elif event == 'push' and ref.startswith('refs/tags/v'):
        sha = os.environ['GITHUB_SHA']
        try:
            green = main_ci_green(sha)
        except Exception as error:  # Unknown: run everything rather than skip.
            print(f'::warning::could not read main CI for {sha}: {error}')
            green = False
        print(f'main CI for {sha}: {"passed, skipping" if green else "not passed, running everything"}')
        flags = dict.fromkeys(FLAGS, not green)
    else:
        flags = dict.fromkeys(FLAGS, True)
    lines = ''.join(f'{k}={str(v).lower()}\n' for k, v in flags.items())
    print(lines, end='')
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a') as out:
            out.write(lines)
    return 0


def main(argv):
    if argv[1:] == ['scope']:
        return scope()
    if len(argv) == 3 and argv[1] == 'main-ci':
        if main_ci_green(argv[2]):
            print(f'ci.yml passed on main for {argv[2]}')
            return 0
        print(f'::error::ci.yml has not passed on main for {argv[2]}. Wait for the main '
              'CI run of this commit to pass, then re-run this workflow run.')
        return 1
    print(f'usage: {argv[0]} scope | main-ci <sha>', file=sys.stderr)
    return 2


if __name__ == '__main__':
    sys.exit(main(sys.argv))
