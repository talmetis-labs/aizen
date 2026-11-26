"""Actual concurrent native-owned CLI acceptance, never general shell/build isolation.
Fresh disposable repo/private home/loopback provider only; all evidence is retained.
"""
import argparse, hashlib, json, os, pathlib, shutil, subprocess, sys, tempfile, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

def require(value, detail):
    if not value:
        raise AssertionError(detail)

def run(binary):
    binary = pathlib.Path(binary).resolve(strict=True)
    artifact = pathlib.Path(tempfile.mkdtemp(prefix='aizen-owned-same-wave-'))
    print('ARTIFACTS ' + str(artifact), flush=True)
    root, home, runs, logs = (artifact / name for name in ('repo', 'home', 'runs', 'logs'))
    for path in (root, home, runs, logs):
        path.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.upper().startswith(('AIZEN_', 'OPENAI_', 'ANTHROPIC_', 'GIT_')) and k.upper() not in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'NO_PROXY')}
    empty = artifact / 'empty-git-config'
    empty.write_bytes(b'')
    env.update(AIZEN_HOME=str(home), AIZEN_RUNS_DIR=str(runs), AIZEN_ARCHITECT='0', NO_PROXY='127.0.0.1', GIT_CONFIG_GLOBAL=str(empty), GIT_CONFIG_NOSYSTEM='1', GIT_OPTIONAL_LOCKS='0')
    gitexe = shutil.which('git')
    require(gitexe, 'Git required')

    def git(*args, cwd=root):
        p = subprocess.run([gitexe, *args], cwd=cwd, env=env, capture_output=True, timeout=30)
        require(p.returncode == 0, (args, p.returncode, p.stderr.decode(errors='replace')))
        return p.stdout

    def cli(label, args):
        print('START ' + label, flush=True)
        p = subprocess.run([str(binary), '--sandbox', 'guarded', *args], cwd=root, env=env, input=b'', capture_output=True, timeout=180)
        (logs / (label + '.stdout')).write_bytes(p.stdout)
        (logs / (label + '.stderr')).write_bytes(p.stderr)
        print(f'END {label} exit={p.returncode}', flush=True)
        return p
    git('init', '-q')
    for key, val in (('user.name', 'Fixture'), ('user.email', 'fixture@example.invalid'), ('core.autocrlf', 'false')):
        git('config', key, val)
    (root / '.aizen').mkdir()
    (root / 'tracked.txt').write_bytes(b'base\n')
    (root / '.gitignore').write_bytes(b'private.tmp\n')
    (root / '.aizen/approvals.json').write_text(json.dumps({'allow': [{'tool': 'file_edit'}]}), encoding='utf-8')
    git('add', '--', 'tracked.txt', '.gitignore', '.aizen/approvals.json')
    git('commit', '-qm', 'base fixture')
    base = git('rev-parse', 'HEAD').decode().strip()
    state = {'steps': {'owned-first': 0, 'owned-second': 0}, 'active': set(), 'max_active': 0, 'events': [], 'errors': []}
    lock = threading.Lock()
    both_reading = threading.Event()

    class Handler(BaseHTTPRequestHandler):

        def log_message(self, *args):
            pass

        def respond(self, data, stream=False):
            wire = json.dumps(data).encode()
            wire = b'data: ' + wire + b'\n\ndata: [DONE]\n\n' if stream else wire
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream' if stream else 'application/json')
            self.send_header('Content-Length', str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

        def do_GET(self):
            require(self.path.startswith('/v1/models'), self.path)
            self.respond({'data': [{'id': m, 'context_window': 200000} for m in state['steps']]})

        def do_POST(self):
            try:
                require(self.path == '/v1/chat/completions', self.path)
                n = int(self.headers.get('Content-Length', '0'))
                require(0 < n < 512 * 1024, n)
                body = json.loads(self.rfile.read(n))
                model = body['model']
                require(model in state['steps'], model)
                with lock:
                    step = state['steps'][model]
                    state['steps'][model] += 1
                    if step == 0:
                        state['active'].add(model)
                    state['max_active'] = max(state['max_active'], len(state['active']))
                    state['events'].append({'model': model, 'step': step, 'time': time.monotonic()})
                    if len(state['active']) == 2:
                        both_reading.set()
                require(step < 3, ('redispatch', model, step))
                names = {tool['function']['name'] for tool in body['tools']}
                require(names and names <= {'file_read', 'file_edit', 'file_write'}, names)
                results = [m for m in body['messages'] if m['role'] == 'tool']
                require(len(results) == step, results)
                require(all((not m['content'].startswith('error:') for m in results)), results)
                text = 'FIRST' if model == 'owned-first' else 'SECOND'
                if step == 0:
                    require(both_reading.wait(30), 'second owned child did not reach the provider while the first was active')
                    name, args = ('file_read', {'path': 'tracked.txt'})
                elif step == 1:
                    require('base' in results[0]['content'] and 'FIRST' not in results[0]['content'] and ('SECOND' not in results[0]['content']), results)
                    name, args = ('file_edit', {'path': 'tracked.txt', 'old_string': 'base', 'new_string': text})
                else:
                    require(results[-1]['content'].startswith('edited tracked.txt'), results)
                    delta, finish = ({'content': 'Changed tracked.txt only; not integrated or verified.'}, 'stop')
                    with lock:
                        state['active'].remove(model)
                if step < 2:
                    delta = {'tool_calls': [{'index': 0, 'id': f'{model}-{step}', 'type': 'function', 'function': {'name': name, 'arguments': json.dumps(args)}}]}
                    finish = 'tool_calls'
                self.respond({'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]}, True)
            except Exception as e:
                state['errors'].append(repr(e))
                self.send_error(400, 'loopback assertion failed')
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    (home / 'cli-config.json').write_text(json.dumps({'base_url': f'http://127.0.0.1:{server.server_port}/v1', 'api_key': 'fixture', 'model': 'owned-first', 'architect_mode': False, 'self_review': False, 'eager_tools': False, 'compact_threshold_pct': 0}), encoding='utf-8')
    spec = artifact / 'same-wave.json'
    spec.write_text(json.dumps({'name': 'same-wave-owned', 'tasks': [{'id': label, 'role': 'daedalus', 'model': 'owned-' + label, 'isolation': 'worktree', 'write_paths': ['tracked.txt'], 'max_steps': 4, 'prompt': 'Read tracked.txt and replace base with ' + text + ' using file_edit. No integration or verification.'} for label, text in [('first', 'FIRST'), ('second', 'SECOND')]]}), encoding='utf-8')

    def snapshot():
        return {'HEAD': git('rev-parse', 'HEAD').hex(), 'index': (root / '.git/index').read_bytes().hex(), 'status': git('status', '--porcelain=v1', '-z').hex(), 'files': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in root.rglob('*') if p.is_file() and '.git' not in p.relative_to(root).parts}}
    try:
        p = cli('trust', ['mcp', 'trust'])
        require(p.returncode == 0, p.stderr)
        (root / 'tracked.txt').write_bytes(b'parent staged\n')
        git('add', '--', 'tracked.txt')
        (root / 'tracked.txt').write_bytes(b'parent unstaged\n')
        (root / 'pending.txt').write_bytes(b'parent untracked\n')
        (root / 'private.tmp').write_bytes(b'private ignored\n')
        dirty = snapshot()
        (artifact / 'dirty-parent-before.json').write_text(json.dumps(dirty, indent=2), encoding='utf-8')
        p = cli('dirty-parent-refused', ['workflow', str(spec), '--yes'])
        require(p.returncode != 0 and b'destination has pending tracked edits' in p.stderr, p.stderr.decode(errors='replace'))
        require(snapshot() == dirty, 'dirty refusal altered parent state')
        require(sum(state['steps'].values()) == 0, state)
        require(len(git('worktree', 'list', '--porcelain').decode().split('worktree ')) - 1 == 1, 'dirty refusal allocated checkout')
        git('restore', '--staged', '--', 'tracked.txt')
        (root / 'tracked.txt').write_bytes(b'base\n')
        (root / 'pending.txt').rename(artifact / 'retained-pending.txt')
        clean = snapshot()
        (artifact / 'clean-parent-before.json').write_text(json.dumps(clean, indent=2), encoding='utf-8')
        p = cli('same-wave', ['workflow', str(spec), '--yes'])
        require(p.returncode != 0 and b'workflow parked:' in p.stderr, p.stderr.decode(errors='replace'))
        require(not state['errors'], state['errors'])
        require(state['steps'] == {'owned-first': 3, 'owned-second': 3}, state)
        require(state['max_active'] == 2, ('two concurrent owned children were not observed', state))
        journals = [p for p in runs.glob('*.jsonl') if 'attempt_completed' in p.read_text(encoding='utf-8')]
        require(len(journals) == 1, journals)
        journal = journals[0]
        events = [json.loads(line) for line in journal.read_text(encoding='utf-8').splitlines()]
        finished = [e for e in events if e['kind'] == 'attempt_completed']
        require(len(finished) == 2, finished)
        require(all((e['status'] == 'git-handoff-pending' for e in finished)), finished)
        handoffs = {e['key']['task_id']: e['git_handoff'] for e in finished}
        require(set(handoffs) == {'first', 'second'}, handoffs)
        require(handoffs['first']['root'] != handoffs['second']['root'] and handoffs['first']['tip'] != handoffs['second']['tip'], handoffs)
        for label, text in [('first', 'FIRST'), ('second', 'SECOND')]:
            h = handoffs[label]
            child = pathlib.Path(h['root'])
            require(h['base'] == base and (not h['dirty']) and (not h['verified']) and (not h['integrated']) and (h['paths'] == ['tracked.txt']) and (h['commits'] == [h['tip']]), h)
            require((child / 'tracked.txt').read_bytes() == (text + '\n').encode(), h)
            require(not (child / 'private.tmp').exists(), 'ignored parent file copied')
            require(git('status', '--porcelain=v1', '-z', cwd=child) == b'', 'dirty child')
            require(git('show', h['tip'] + ':tracked.txt', cwd=child) == (text + '\n').encode(), h)
            require(pathlib.Path(h['root']).resolve() != root.resolve(), 'child routed to parent')
        require(not any((e['kind'] in ('git_task_accepted', 'task_settled') for e in events)), 'handoff settled without verification')
        after = snapshot()
        (artifact / 'clean-parent-after.json').write_text(json.dumps(after, indent=2), encoding='utf-8')
        require(after == clean, 'same-wave dispatch altered parent')
        p = cli('resume-no-redispatch', ['run-resume', journal.stem, '--spec', str(spec), '--yes'])
        require(p.returncode != 0 and b'workflow parked:' in p.stderr, p.stderr.decode(errors='replace'))
        require(state['steps'] == {'owned-first': 3, 'owned-second': 3} and snapshot() == clean, (state, 'resume changed state'))
        result = {'status': 'passed', 'binary': str(binary), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'evidence': str(artifact), 'dispatch': 'two independent same-wave native-owned children overlapped behind a loopback barrier in distinct fresh roots; no shell/build isolation claim', 'max_active_children': state['max_active'], 'checks': 'two exact-owned roots, same relative filename and base; distinct committed contents; parent dirty refusal and clean dispatch preserve exact bytes/HEAD/index; ignored files not copied; no acceptance/settlement before destination verification; resume does not redispatch', 'handoffs': handoffs, 'provider_events': state['events']}
        (artifact / 'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
        print(json.dumps(result, indent=2), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        (artifact / 'provider-state.json').write_text(json.dumps({**state, 'active': sorted(state['active'])}, indent=2), encoding='utf-8')
if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True)
    args = parser.parse_args()
    run(args.binary)
