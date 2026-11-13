"""Cold lookups through the real CLI, with and without the .fence file: same rows, fewer disk reads.
   python3 -I scripts/fence_check.py --csv data/orders.csv --keys data/keys.txt [--n 100]"""
import argparse, os, resource, statistics, subprocess, time

ap = argparse.ArgumentParser()
ap.add_argument("--csv", required=True); ap.add_argument("--keys", required=True)
ap.add_argument("--n", type=int, default=100); ap.add_argument("--bin", default="./target/release/csvidx")
a = ap.parse_args()
idx = a.csv + ".idx"; fence = idx + ".fence"; off = fence + ".off"
keys = [k for k in open(a.keys).read().split() if k][: a.n]

def evict():
    for p in (a.csv, idx, fence):
        if os.path.exists(p):
            fd = os.open(p, os.O_RDONLY); os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED); os.close(fd)

def run(use_fence):
    if use_fence != os.path.exists(fence):
        os.rename(off, fence) if use_fence else os.rename(fence, off)
    times, faults, outs = [], 0, []
    for k in keys:
        evict()
        before = resource.getrusage(resource.RUSAGE_CHILDREN).ru_majflt
        t = time.perf_counter()
        r = subprocess.run([a.bin, "get", a.csv, "--", k], capture_output=True, check=True)
        times.append((time.perf_counter() - t) * 1e3)
        faults += resource.getrusage(resource.RUSAGE_CHILDREN).ru_majflt - before
        outs.append(r.stdout)
    return times, faults / len(keys), outs

assert os.path.exists(fence), "no fence file: run `csvidx build` first"
w, wf, wo = run(True)
n, nf, no = run(False)
os.rename(off, fence)
assert wo == no, "OUTPUT DIFFERS with and without fences"
print(f"{len(keys)} cold lookups, identical output")
print(f"with fence   : median {statistics.median(w):7.1f} ms   major faults/lookup {wf:5.1f}")
print(f"without fence: median {statistics.median(n):7.1f} ms   major faults/lookup {nf:5.1f}")
