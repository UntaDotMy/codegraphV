"""Non-network lifecycle probes with isolated home/project; never uninstall binary."""
import json
import os
import pathlib
import shutil
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = pathlib.Path(__file__).resolve().parent
results = {}
with tempfile.TemporaryDirectory(prefix='codegraphV-audit-') as tmp:
    base = pathlib.Path(tmp)
    exe = base / 'codegraphV.exe'
    shutil.copy2(ROOT / 'target/debug/codegraphV.exe', exe)
    env = os.environ.copy()
    home = base / 'home'
    home.mkdir()
    for key in ['HOME','USERPROFILE','APPDATA','LOCALAPPDATA','XDG_CONFIG_HOME','CODEX_HOME','CLAUDE_CONFIG_DIR','COPILOT_HOME','HERMES_HOME']:
        env[key] = str(home / key)
        pathlib.Path(env[key]).mkdir()
    env['CODEGRAPHV_NO_UPDATE_CHECK'] = '1'
    env['DO_NOT_TRACK'] = '1'
    def run(project, *args):
        p = subprocess.run([str(exe),*args], cwd=project, env=env, text=True, capture_output=True, timeout=40)
        if p.returncode:
            raise RuntimeError((args,p.returncode,p.stdout,p.stderr))
        return p.stdout
    results['version'] = run(base, '--version').strip()
    p = base/'permissions'; p.mkdir()
    run(p,'install','--target','claude','--location','local','--yes','--no-permissions')
    path = p/'.claude/settings.json'
    before = json.loads(path.read_text())
    run(p,'install','--target','claude','--location','local','--refresh')
    after = json.loads(path.read_text())
    results['refresh_permissions'] = dict(before=before.get('permissions'),after=after.get('permissions'))
    p = base/'ownership'; p.mkdir(); (p/'.claude').mkdir()
    path = p/'.claude/settings.json'
    original = 'echo codegraphV prompt-hook'
    path.write_text(json.dumps({'hooks':{'Stop':[{'hooks':[{'type':'command','command':original}]}]}}))
    run(p,'uninstall','--target','claude','--location','local','--yes','--keep-cli')
    results['unrelated_hook_removal'] = dict(before=original,after=path.read_text() if path.exists() else None)
    p = base/'shared-instructions'; p.mkdir()
    run(p,'install','--target','codex,opencode','--location','local','--yes')
    before = (p/'AGENTS.md').read_text()
    run(p,'uninstall','--target','codex','--location','local','--yes','--keep-cli')
    results['shared_instructions'] = dict(before_has_block='CODEGRAPHV_START' in before,after_exists=(p/'AGENTS.md').exists(),opencode_still_configured='codegraphV' in (p/'opencode.jsonc').read_text())
    preservation = []
    for target, relative in [('claude','.claude/CLAUDE.md'),('codex','AGENTS.md'),('opencode','AGENTS.md'),('gemini','GEMINI.md')]:
        for variant, original in [('lf',b'# User rules\n\nKeep my rules.\n'),('crlf',b'# User rules\r\n\r\nKeep my rules.\r\n'),('no-final-newline',b'# User rules\nKeep my rules.'),('whitespace-only',b' \t\r\n\r\n')]:
            p = base/f'preserve-{target}-{variant}'; p.mkdir()
            path = p/relative; path.parent.mkdir(exist_ok=True)
            path.write_bytes(original)
            run(p,'install','--target',target,'--location','local','--yes')
            first = path.read_bytes()
            run(p,'install','--target',target,'--location','local','--refresh')
            second = path.read_bytes()
            run(p,'uninstall','--target',target,'--location','local','--yes','--keep-cli')
            after = path.read_bytes() if path.exists() else None
            preservation.append(dict(target=target,variant=variant,install_keeps_original_prefix=first.startswith(original),refresh_byte_identical=first==second,managed_blocks=first.count(b'<!-- CODEGRAPHV_START -->'),uninstall_exact_original=after==original,after_exists=after is not None))
    results['instructions_preservation'] = preservation
    p = base/'fenced-example'; p.mkdir()
    path = p/'AGENTS.md'
    original = '# User documentation\n\n```markdown\n<!-- CODEGRAPHV_START -->\nEXAMPLE ONLY: retain this text\n<!-- CODEGRAPHV_END -->\n```\n'
    path.write_text(original)
    run(p,'install','--target','codex','--location','local','--yes')
    results['fenced_marker_example'] = dict(example_preserved='EXAMPLE ONLY: retain this text' in path.read_text(),managed_block_inside_fence='```markdown\n<!-- CODEGRAPHV_START -->\n## CodeGraphV' in path.read_text())
    p = base/'sibling-hooks'; p.mkdir(); (p/'.claude').mkdir()
    path = p/'.claude/settings.json'
    user_hooks = {'UserPromptSubmit':[{'matcher':'','hooks':[{'type':'command','command':'my-user-hook','timeout':7}]}], 'Stop':[{'hooks':[{'type':'command','command':'my-stop-hook'}]}]}
    original_settings = {'hooks':user_hooks,'permissions':{'allow':['Bash(ls)']},'unrelatedSetting':'keep'}
    path.write_text(json.dumps(original_settings))
    run(p,'install','--target','claude','--location','local','--yes')
    installed = json.loads(path.read_text())
    run(p,'install','--target','claude','--location','local','--refresh')
    refreshed = json.loads(path.read_text())
    run(p,'uninstall','--target','claude','--location','local','--yes','--keep-cli')
    removed = json.loads(path.read_text())
    results['ordinary_hook_siblings'] = dict(user_group_preserved=installed['hooks']['UserPromptSubmit'][0]==user_hooks['UserPromptSubmit'][0],stop_preserved=installed['hooks']['Stop']==user_hooks['Stop'],prompt_groups_after_install=len(installed['hooks']['UserPromptSubmit']),refresh_unchanged=refreshed==installed,uninstall_restores_settings_values=removed==original_settings)
(OUT/'lifecycle-probes.json').write_text(json.dumps(results,indent=2),encoding='utf-8')
print(json.dumps(results,indent=2))
