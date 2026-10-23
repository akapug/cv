#!/usr/bin/env python3
"""Profile one executable operation twice; OUT must be new and task-owned."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def overlaps(a, b):
    return a == b or a in b.parents or b in a.parents


def fingerprint(path):
    """Hash file bytes, or sorted directory-relative paths plus bytes (never follow links)."""
    if path.is_file():
        return hashlib.sha256(path.read_bytes()).hexdigest()
    digest = hashlib.sha256()
    for file in sorted(path.rglob('*')):
        if file.is_symlink():
            raise ValueError('fixture/seed directory must not contain symlinks')
        if file.is_file():
            name = str(file.relative_to(path)).encode()
            digest.update(len(name).to_bytes(8, 'little'))
            digest.update(name)
            digest.update(file.stat().st_size.to_bytes(8, 'little'))
            with file.open('rb') as stream:
                for part in iter(lambda: stream.read(65536), b''):
                    digest.update(part)
    return digest.hexdigest()


def metadata_fingerprint(path):
    entries = [path, *sorted(path.rglob('*'))] if path.is_dir() else [path]
    states = []
    for entry in entries:
        st = entry.stat()
        states.append((str(entry.relative_to(path)) if entry != path else '.',
                       st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns))
    return hashlib.sha256(json.dumps(states).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, required=True, help='fixture file or directory whose SHA256 is recorded')
    parser.add_argument('--output', choices=['probe-json', 'bytes'], default='probe-json')
    parser.add_argument('--expect-stderr', action='append', default=[], help='required child diagnostic substring')
    parser.add_argument('--seed-catalog', type=Path, help='frozen scratch catalog/index directory restored before every child')
    parser.add_argument('--catalog-home', type=Path, help='initially absent task-owned restoration path; retain index namespace')
    parser.add_argument('--harness-home', type=Path, help='isolated synthetic/scrubbed harness HOME')
    parser.add_argument('--threads', type=int, default=1, help='explicit Rayon worker count')
    parser.add_argument('out', type=Path)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command and command[0] == '--':
        command = command[1:]
    if not command:
        parser.error('provide an executable command after --')
    if args.threads < 1:
        parser.error('--threads must be positive')
    executable = shutil.which(command[0])
    if executable is None:
        parser.error('command executable does not exist or is not executable')
    command[0] = str(Path(executable).resolve())
    fixture = args.input.resolve(strict=True)
    seed = args.seed_catalog.resolve(strict=True) if args.seed_catalog else None
    harness_home = args.harness_home.resolve(strict=True) if args.harness_home else None
    if seed and not seed.is_dir():
        parser.error('--seed-catalog must be a directory')
    if harness_home and not harness_home.is_dir():
        parser.error('--harness-home must be a directory')
    catalog_home = args.catalog_home.resolve() if args.catalog_home else None
    if catalog_home and catalog_home.exists():
        parser.error('--catalog-home must initially be absent; this runner owns only new paths')
    protected = [p for p in (fixture, seed, harness_home) if p]
    if catalog_home and any(overlaps(catalog_home, p) for p in protected):
        parser.error('--catalog-home cannot overlap fixture, seed or harness HOME')
    if any(overlaps(args.out.resolve(), p) for p in protected):
        parser.error('output directory cannot overlap fixture, seed or harness HOME')
    if catalog_home and (catalog_home == args.out.resolve() or catalog_home in args.out.resolve().parents):
        parser.error('--catalog-home cannot contain the output directory')
    input_hash = fingerprint(fixture)
    input_metadata = metadata_fingerprint(fixture)
    seed_hash = fingerprint(seed) if seed else None
    seed_metadata = metadata_fingerprint(seed) if seed else None
    args.out.mkdir(parents=True, exist_ok=False)
    out = args.out.resolve()
    home = catalog_home or out / 'catalog'
    env = dict(os.environ, CLUSTERVISION_HOME=str(home), LC_ALL='C', TZ='UTC',
               RAYON_NUM_THREADS=str(args.threads))
    if harness_home:
        env.update(HOME=str(harness_home), XDG_CONFIG_HOME=str(out / 'config'),
                   XDG_DATA_HOME=str(out / 'data'), XDG_CACHE_HOME=str(out / 'cache'))
    outputs = []
    diagnostics = []
    counts = []
    for label in ('native', '1', '2'):
        if home.exists():
            shutil.rmtree(home)  # Only this runner's newly created scratch catalog.
        if seed:
            shutil.copytree(seed, home)
        else:
            home.mkdir()
        argv = command
        if label != 'native':
            argv = ['valgrind', '--tool=cachegrind', '--cache-sim=no', '--branch-sim=no',
                    '--log-file=' + str(out / f'valgrind-{label}.log'),
                    '--cachegrind-out-file=' + str(out / f'cachegrind-{label}.out'), *command]
        result = subprocess.run(argv, env=env, check=True, capture_output=True)
        suffix = 'json' if args.output == 'probe-json' else 'out'
        (out / f'stdout-{label}.{suffix}').write_bytes(result.stdout)
        (out / f'stderr-{label}.txt').write_bytes(result.stderr)
        if any(text.encode() not in result.stderr for text in args.expect_stderr):
            raise RuntimeError('required child diagnostic missing')
        diagnostics.append(result.stderr)
        semantic = result.stdout
        if args.output == 'probe-json':
            semantic = json.loads(result.stdout)
            semantic.pop('wall_seconds', None)  # Sole nondeterministic probe instrumentation field.
        outputs.append(semantic)
        if label != 'native':
            profile = (out / f'cachegrind-{label}.out').read_text()
            events = next(line.split()[1:] for line in profile.splitlines() if line.startswith('events:'))
            totals = next(line.split()[1:] for line in profile.splitlines() if line.startswith('summary:'))
            counts.append(int(dict(zip(events, totals))['Ir']))
            with (out / f'annotate-{label}.txt').open('w') as report:
                subprocess.run(['cg_annotate', '--auto=no', '--show=Ir', str(out / f'cachegrind-{label}.out')],
                               stdout=report, check=True, env=env)
    if args.output == 'bytes' and not diagnostics[0] == diagnostics[1] == diagnostics[2]:
        raise RuntimeError('native/profiled stderr bytes differ')
    if outputs[0] != outputs[1] or outputs[1] != outputs[2]:
        raise RuntimeError('native/profiled semantic output differs')
    with (out / 'diff.out').open('w') as report:
        subprocess.run(['cg_diff', str(out / 'cachegrind-1.out'), str(out / 'cachegrind-2.out')],
                       stdout=report, check=True, env=env)
    with (out / 'annotate-diff.txt').open('w') as report:
        subprocess.run(['cg_annotate', '--auto=no', '--show=Ir', '--threshold=0', str(out / 'diff.out')],
                       stdout=report, check=True, env=env)
    if (fingerprint(fixture) != input_hash or metadata_fingerprint(fixture) != input_metadata
            or (seed and (fingerprint(seed) != seed_hash or metadata_fingerprint(seed) != seed_metadata))):
        raise RuntimeError('input or frozen seed changed during profiling')
    binary = Path(command[0]).resolve()
    summary = {'command': command, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
               'input_sha256': input_hash, 'input_metadata_sha256': input_metadata,
               'seed_catalog_sha256': seed_hash, 'seed_metadata_sha256': seed_metadata, 'output_mode': args.output,
               'catalog_home': str(home),
               'harness_home': str(harness_home) if harness_home else None,
               'rayon_threads': args.threads,
               'runtime_rustflags': os.environ.get('RUSTFLAGS', ''),
               'catalog_max_stale_secs': env.get('CLUSTERVISION_MAX_STALE_SECS', '900'),
               'valgrind': subprocess.check_output(['valgrind', '--version'], text=True).strip(),
               'architecture': os.uname().machine, 'Ir': counts, 'Ir_delta': counts[1] - counts[0],
               'stderr_output': {'sha256': hashlib.sha256(diagnostics[0]).hexdigest(), 'bytes': len(diagnostics[0])},
               'semantic_output': ({'sha256': hashlib.sha256(outputs[0]).hexdigest(), 'bytes': len(outputs[0])}
                                   if args.output == 'bytes' else outputs[0]), 'parity': True}
    (out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
