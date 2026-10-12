#!/usr/bin/env python3
"""Read fixed maintained DHI agent payload identities without executing them.

The Claude version is the one versions/ entry the template ships, so a base
repin needs no edit here.
"""
import hashlib,json,pathlib
versions=pathlib.Path('/home/agent/.local/share/claude/versions');npm=pathlib.Path('/usr/local/share/npm-global/lib/node_modules/@openai')
def sha(p):
 h=hashlib.sha256()
 with p.open('rb') as f:
  while chunk:=f.read(1024*1024):h.update(chunk)
 return h.hexdigest()
files=[]
if versions.is_dir():
 shipped=sorted(versions.iterdir())
 if len(shipped)!=1 or not shipped[0].is_file() or shipped[0].is_symlink():raise ValueError('expected exactly one maintained Claude version file')
 claude=shipped[0];files.append({'path':str(claude),'sha256':sha(claude),'size':claude.stat().st_size})
packages=[]
if npm.is_dir():
 for p in npm.rglob('package.json'):
  if p.stat().st_size>1024*1024:raise ValueError('oversized package metadata')
  d=json.loads(p.read_text());packages.append({'path':str(p),'name':d.get('name'),'version':d.get('version'),'license':d.get('license'),'sha256':sha(p)})
 for p in npm.rglob('*'):
  if p.is_file() and (p.name in ('codex','codex-code-mode-host','LICENSE','LICENSE.md','NOTICE') or p.suffix=='.node'):
   files.append({'path':str(p),'sha256':sha(p),'size':p.stat().st_size})
print(json.dumps({'schema':'marsh.dhi-agent-inventory/v1','packages':packages,'files':files},indent=2))
