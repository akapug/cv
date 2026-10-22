#!/usr/bin/env python3
"""Verify one successful release run and smoke its native Darwin binary in an isolated home."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def api(repo, route):
    result = subprocess.run(['gh', 'api', '--hostname', 'github.com', f'repos/{repo}/{route}'],
                            check=True, capture_output=True, timeout=60)
    return json.loads(result.stdout)


def validate_run(run, repo, run_id, tag, sha):
    require(re.fullmatch(r'v\d+\.\d+\.\d+-rc(?:[0-9]+|(?:\.[0-9A-Za-z-]+)+)', tag), 'expected an exact rc tag')
    require(re.fullmatch(r'[0-9a-f]{40}', sha), 'expected full source SHA')
    require(run['id'] == run_id and run['repository']['full_name'] == repo
            and run['head_repository']['full_name'] == repo, 'wrong release run/repository')
    require(run['path'] == '.github/workflows/release.yml' and run['event'] == 'push'
            and run['status'] == 'completed' and run['conclusion'] == 'success', 'release run not successful')
    require(run['head_branch'] == tag and run['head_sha'] == sha, 'wrong release run tag/SHA')


def tag_sha(repo, tag):
    obj = api(repo, f'git/ref/tags/{tag}')['object']
    for _ in range(8):
        if obj['type'] == 'commit':
            return obj['sha']
        require(obj['type'] == 'tag', 'tag does not resolve to commit')
        obj = api(repo, f"git/tags/{obj['sha']}")['object']
    raise RuntimeError('tag nesting exceeds bound')


def select_asset(release, tag, target):
    require(release['tag_name'] == tag and release['prerelease'] and not release['draft'],
            'wrong/unpublished rc release')
    name = f'clustervision-{target}.tar.xz'
    assets = [a for a in release['assets'] if a['name'] == name]
    require(len(assets) == 1, 'missing/ambiguous Darwin archive')
    asset = assets[0]
    require(asset['state'] == 'uploaded' and 0 < asset['size'] <= 200_000_000, 'invalid asset state/size')
    require(re.fullmatch(r'sha256:[0-9a-f]{64}', asset.get('digest') or ''), 'missing asset SHA256 digest')
    return asset


def extract_binary(archive, asset, target, directory):
    require(archive.stat().st_size == asset['size'], 'asset size mismatch')
    require(hashlib.sha256(archive.read_bytes()).hexdigest() == asset['digest'][7:], 'asset digest mismatch')
    root = f'clustervision-{target}'
    with tarfile.open(archive, 'r:xz') as package:
        members = package.getmembers()
        require(len(members) <= 1000, 'too many archive members')
        seen = set()
        for member in members:
            path = PurePosixPath(member.name)
            require(not path.is_absolute() and '..' not in path.parts and path.parts
                    and path.parts[0] == root, 'unsafe archive path')
            require(path not in seen and (member.isfile() or member.isdir()),
                    'duplicate or non-regular archive member')
            seen.add(path)
        binaries = [m for m in members if PurePosixPath(m.name) == PurePosixPath(root)/'cv' and m.isfile()]
        require(len(binaries) == 1 and 0 < binaries[0].size <= 200_000_000, 'missing/invalid cv binary')
        binary = directory/'cv'
        # Never extract archive paths or honor links/modes; publish only the checked binary.
        with package.extractfile(binaries[0]) as source, binary.open('xb') as output:
            shutil.copyfileobj(source, output)
    binary.chmod(0o700)
    return binary


def smoke(binary, tag, sha, root, fixture):
    home = root/'home'
    home.mkdir()
    shutil.copytree(fixture/'.claude', home/'.claude')
    for name in ('tmp', 'catalog', 'cache'):
        (root/name).mkdir()
    env = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(home), 'NO_COLOR': '1',
           'CLUSTERVISION_HOME': str(root/'catalog'), 'TMPDIR': str(root/'tmp'),
           'XDG_CONFIG_HOME': str(home/'.config'), 'XDG_DATA_HOME': str(home/'.local/share'),
           'XDG_CACHE_HOME': str(root/'cache'), 'CLAUDE_CONFIG_DIR': str(home/'.claude'),
           'CODEX_HOME': str(home/'.codex')}
    def run(*args):
        return subprocess.run([str(binary), *args], env=env, cwd=root, check=True,
                              capture_output=True, timeout=120).stdout
    def snapshot():
        return {str(p.relative_to(home)): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in (home/'.claude').rglob('*') if p.is_file()}
    before = snapshot()
    version = run('--version').decode().strip()
    require(re.search(r'(?<![\w.])'+re.escape(tag[1:])+r'(?![\w.-])', version), 'wrong binary version')
    built = re.search(r'\(([0-9a-f]{12,40})\)', version)
    require(built and sha.startswith(built[1]), 'wrong binary source SHA')
    rows = json.loads(run('ls', '--json'))
    require(isinstance(rows, list) and rows, 'empty/invalid discovery')
    row = next((r for r in rows if r.get('harness') == 'claude'), None)
    require(row is not None and isinstance(row.get('id'), str), 'missing fixture session')
    output = run('show', 'claude:'+row['id'], '--json')
    shown = json.loads(output)
    require(isinstance(shown, dict) and isinstance(shown.get('messages'), list) and shown['messages'],
            'empty/invalid session output')
    require(snapshot() == before, 'smoke modified transcript fixture')
    return {'version': version, 'rows': len(rows), 'show_sha256': hashlib.sha256(output).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--run-id', required=True, type=int)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--sha', required=True)
    parser.add_argument('--target', required=True, choices=['aarch64-apple-darwin', 'x86_64-apple-darwin'])
    args = parser.parse_args()
    require(re.fullmatch(r'[\w.-]+/[\w.-]+', args.repo), 'invalid repository')
    run = api(args.repo, f'actions/runs/{args.run_id}')
    validate_run(run, args.repo, args.run_id, args.tag, args.sha)
    require(tag_sha(args.repo, args.tag) == args.sha, 'release tag moved/wrong source SHA')
    native = {'aarch64-apple-darwin': 'arm64', 'x86_64-apple-darwin': 'x86_64'}[args.target]
    require(platform.system() == 'Darwin' and platform.machine() == native, 'not the native Darwin architecture')
    release = api(args.repo, f'releases/tags/{args.tag}')
    asset = select_asset(release, args.tag, args.target)
    root = Path(tempfile.mkdtemp(prefix='cv-rc-smoke-', dir=os.environ['RUNNER_TEMP']))
    print(f'rc smoke scratch: {root}', flush=True)
    archive = root/asset['name']
    with archive.open('xb') as output:
        subprocess.run(['gh', 'api', '--hostname', 'github.com', '-H', 'Accept: application/octet-stream',
                        f"repos/{args.repo}/releases/assets/{asset['id']}"], check=True, stdout=output, timeout=180)
    binary = extract_binary(archive, asset, args.target, root)
    result = smoke(binary, args.tag, args.sha, root, Path(__file__).resolve().parents[1]/
                   'crates/cv-core/tests/corpus/claude/2.1.296')
    require(tag_sha(args.repo, args.tag) == args.sha, 'release tag moved during smoke')
    result.update(run_id=args.run_id, tag=args.tag, sha=args.sha, release_id=release['id'],
                  asset_id=asset['id'], digest=asset['digest'], target=args.target)
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
