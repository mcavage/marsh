#!/usr/bin/env python3
"""Replay P1-P4 against an explicit pre-repair snapshot, never the integration tree.

Uses the same real fixture process/daemon socket Rig as fixture_caller.rs. This is
not a release gate; expected assertion failures are diagnostic red evidence.
"""
import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile
import os
import hashlib
import json

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--baseline', type=Path, required=True)
p.add_argument('--target', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
a=p.parse_args()
if a.target.exists() and any(a.target.iterdir()):
    p.error('--target must be fresh and empty; copying older source mtimes into a reused Cargo target is not a valid red oracle')
source=Path(__file__).resolve().parents[3]
with tempfile.TemporaryDirectory(prefix='marsh-codex-acp-red-') as temp:
    root=Path(temp)
    for name in ['Cargo.toml','Cargo.lock','rust-toolchain.toml']:
        shutil.copy2(source/name,root/name)
    for name in ['vendor','tests']:
        (root/name).symlink_to(source/name,target_is_directory=True)
    (root/'crates').mkdir()
    for crate in (source/'crates').iterdir():
        if crate.name in {'marsh-acp','marsh-daemon','marsh-mcp'}:
            shutil.copytree(crate,root/'crates'/crate.name)
        else:
            (root/'crates'/crate.name).symlink_to(crate,target_is_directory=True)
    baseline_files = ['crates/marsh-acp/src/client.rs','crates/marsh-daemon/src/acp_session.rs','crates/marsh-mcp/src/acp_export.rs']
    for name in baseline_files:
        shutil.copy2(a.baseline/name,root/name)
    a.output.with_suffix('.identity.json').write_text(json.dumps({
        'baseline': str(a.baseline.resolve()), 'fresh_target': str(a.target.resolve()),
        'baseline_sha256': {name: hashlib.sha256((root/name).read_bytes()).hexdigest() for name in baseline_files},
        'fixture_sha256': hashlib.sha256((source/'tests/acceptance/acp-fixture/agent.mjs').read_bytes()).hexdigest(),
    }, indent=2)+'\n')
    test=(source/'crates/marsh-acp/tests/fixture_caller.rs').read_text().split('#[tokio::test')[0]
    test += '''
#[test]
fn red_idle_cursor() { let r=Rig::new(); assert_eq!(r.status(0).next_cursor,0); }
#[test]
fn red_loss_reset() { let r=Rig::new();r.prompt("lossy");let s=r.done();assert!(s.updates_lost);let start=s.latest_cursor;r.prompt("clean");r.done();assert!(!r.status(start).updates_lost); }
#[test]
fn red_raw_diff() { let r=Rig::new();r.prompt("diff");let s=r.done();assert!(s.updates.iter().any(|u| u.update["content"][0]["newText"]=="new")); }
#[test]
fn red_burst() { let r=Rig::new();r.prompt("burst-400");let s=r.done();assert!(!s.updates_lost);let mut cursor=0;let mut count=0;loop{let s=r.status(cursor);count+=s.updates.len();cursor=s.next_cursor.saturating_sub(1);if !s.more_updates{break}}assert_eq!(count,401); }
'''
    (root/'crates/marsh-acp/tests/red_fixture.rs').write_text(test)
    env=os.environ.copy()
    env.update(CARGO_TARGET_DIR=str(a.target.resolve()),CARGO_INCREMENTAL='0',CARGO_PROFILE_DEV_DEBUG='0',CARGO_PROFILE_TEST_DEBUG='0')
    result=subprocess.run(['cargo','test','-p','marsh-acp','--test','red_fixture','--','--nocapture','--test-threads=1'],cwd=root,env=env,capture_output=True,text=True,timeout=180)
    text=result.stdout+result.stderr
    a.output.write_text(text)
    expected=['red_idle_cursor','red_loss_reset','red_raw_diff']
    if not all(f'test {name} ... FAILED' in text for name in expected):
        raise SystemExit('red probes did not each fail their behavior assertions; inspect '+str(a.output))
    # The unconstrained Node burst may pass on the old implementation depending
    # on scheduling. A paused caller with capacity one deterministically probes
    # backpressure rather than relying on that schedule.
    delayed=(source/'crates/marsh-acp/tests/capture_backpressure.rs').read_text().replace('.prompt_raw(', '.prompt(')
    (root/'crates/marsh-acp/tests/red_delay.rs').write_text(delayed)
    result=subprocess.run(['cargo','test','-p','marsh-acp','--test','red_delay','delayed_consumer_preserves_every_burst_chunk','--','--nocapture'],cwd=root,env=env,capture_output=True,text=True,timeout=180)
    text=result.stdout+result.stderr
    a.output.with_name('acp-delayed-consumer-red.log').write_text(text)
    if 'test delayed_consumer_preserves_every_burst_chunk ... FAILED' not in text:
        raise SystemExit('delayed-consumer red did not fail its behavior assertion')
