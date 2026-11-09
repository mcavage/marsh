#!/usr/bin/env python3
"""TEST-PLAN race/: swap the bind source between a directory and escaping
symlinks while containers are created against it. Runs in the sandbox VM.

  race.py SCOPE_SOCKET PROJECT SECONDS WORKERS CANARY_PATH

Two sources are attacked at once:
  race/d  a real directory, repeatedly renamed away and replaced by a symlink
          to /, ../../.., /proc/self/root, ... (4 syscalls per flip)
  race/s  always a symlink, atomically re-pointed (rename over it) between the
          real directory and an escape
Escape = a container printed the canary that lives only on the engine's root.
The run fails if the flippers did not actually flip.
"""
import collections, os, shutil, subprocess, sys, threading, time

sock, proj, secs, workers, canary = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
base = f"{proj}/race"
d, d_alt = f"{base}/d", f"{base}/d.real"
s, s_real, s_tmp = f"{base}/s", f"{base}/s.real", f"{base}/s.tmp"
for p in (d, d_alt, s, s_real, s_tmp):
    if os.path.islink(p) or os.path.isfile(p):
        os.unlink(p)
    elif os.path.isdir(p):
        shutil.rmtree(p)
os.makedirs(d)
open(f"{d}/ok.txt", "w").write("ok-file\n")
os.makedirs(s_real)
open(f"{s_real}/ok.txt", "w").write("ok-file\n")
os.symlink("s.real", s)
stop = threading.Event()
count = collections.Counter()
lock = threading.Lock()
targets = ["/", "../../../../../../../..", "/proc/self/root", "/etc/..", "..", "../.."]


def note(k, err=None):
    with lock:
        count[k] += 1
        if err and count[k] <= 3:
            print(k, err, flush=True)


def flip_dir():
    i = 0
    while not stop.is_set():
        try:
            os.rename(d, d_alt)
            os.symlink(targets[i % len(targets)], d)
            time.sleep(0.0003)
            os.unlink(d)
            os.rename(d_alt, d)
            note("flips-dir")
        except OSError as e:
            note("flip-errors", e)
            if os.path.islink(d):
                os.unlink(d)
            if not os.path.exists(d) and os.path.exists(d_alt):
                os.rename(d_alt, d)
        i += 1


def flip_symlink():
    i = 0
    while not stop.is_set():
        try:
            os.symlink(targets[i % len(targets)] if i % 2 else "s.real", s_tmp)
            os.rename(s_tmp, s)
            note("flips-symlink")
        except OSError as e:
            note("flip-errors", e)
            try:
                os.unlink(s_tmp)
            except OSError:
                pass
        i += 1
        time.sleep(0.0003)


def worker(n):
    env = dict(os.environ, DOCKER_HOST=f"unix://{sock}")
    k = n
    while not stop.is_set():
        src = d if k % 2 == 0 else s
        k += 1
        r = subprocess.run(["docker", "run", "--rm", "--mount", f"type=bind,src={src},dst=/d", "busybox", "sh", "-c",
                            f"cat /d/ok.txt; cat /d/{canary.lstrip('/')} 2>/dev/null; ls /d/var/lib/docker 2>/dev/null | head -1"],
                           capture_output=True, text=True, env=env)
        out = r.stdout + r.stderr
        with lock:
            if "VMROOT-CANARY" in out:
                count["ESCAPE"] += 1
                print("ESCAPE:", src, out[:200], flush=True)
            elif "ok-file" in out:
                count["pinned-original"] += 1
            elif "marsh:" in out:
                count["refused"] += 1
            else:
                count["other"] += 1
                if count["other"] <= 3:
                    print("other:", out[:200], flush=True)


ts = [threading.Thread(target=flip_dir), threading.Thread(target=flip_symlink)] + [threading.Thread(target=worker, args=(i,)) for i in range(workers)]
t0 = time.time()
for t in ts:
    t.start()
time.sleep(secs)
stop.set()
for t in ts:
    t.join()
for p in (d, s):
    if os.path.islink(p):
        os.unlink(p)
if not os.path.exists(d) and os.path.exists(d_alt):
    os.rename(d_alt, d)
attempts = sum(v for k, v in count.items() if k in ("pinned-original", "refused", "other", "ESCAPE"))
print({"seconds": round(time.time() - t0), "attempts": attempts, **dict(count)})
# No escape is only worth something if the original really was read (a run in
# which every attempt was refused or failed proves nothing), and if the flippers
# really flipped.
MIN_READS = max(10, secs // 4)
checks = [
    (count["ESCAPE"] == 0, f"no container read the VM-root canary ({count['ESCAPE']})"),
    (count["pinned-original"] >= MIN_READS, f"at least {MIN_READS} containers read the ORIGINAL file through the pin ({count['pinned-original']})"),
    (count["flips-dir"] >= 100 and count["flips-symlink"] >= 100, f"the flippers flipped (dir {count['flips-dir']}, symlink {count['flips-symlink']})"),
    (count["other"] <= max(5, attempts // 10), f"unexplained outcomes are rare ({count['other']} of {attempts})"),
]
for ok, label in checks:
    print(("ok   " if ok else "BAD  ") + label)
sys.exit(0 if all(ok for ok, _ in checks) else 1)
