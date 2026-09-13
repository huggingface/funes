"""Measure one prebuilt push workload on macOS; no third-party Python dependencies.

Usage: python benchmark/measure_push.py --output /tmp/new-run --scratch /tmp/run-scratch \
    -- /absolute/path/test-binary benchmark_push_preparation --ignored --nocapture

Use a dedicated scratch directory and set TMPDIR to it for the workload. Command
arguments/output are recorded; never pass secrets or print private transcript text.
Footprint/RSS are process-tree samples including the time wrapper, not guaranteed
peaks. System swap/free space are global guards, not workload attribution. Native time output
contains exit-accounted CPU and a main-process lifetime footprint where supported;
neither its RSS nor its footprint is a simultaneous process-tree total. Sampling
can miss short-lived children and shared mappings may overlap in summed values.

Sources:
https://github.com/apple-oss-distributions/shell_cmds/blob/main/time/time.c
https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_resource.c
https://developer.apple.com/videos/play/wwdc2022/10106/
"""
import argparse
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import time

GIB = 1024**3
TIME = "/usr/bin/time"


class Usage(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [
        (name, ctypes.c_uint64) for name in (
            "user_ticks", "system_ticks", "idle_wakeups", "interrupt_wakeups",
            "pageins", "wired_bytes", "rss_bytes", "footprint_bytes",
            "start_ticks", "exit_ticks")]


def process_usage(lib, pid):
    value = Usage()
    if lib.proc_pid_rusage(pid, 0, ctypes.byref(value)):
        code = ctypes.get_errno()
        if code == errno.ESRCH:  # The process exited between enumeration and sampling.
            return None
        raise OSError(code, f"Cannot measure process {pid}")
    return {name: getattr(value, name) for name in (
        "rss_bytes", "footprint_bytes", "start_ticks")}


def members(root):
    output = subprocess.check_output(
        ["ps", "-axo", "pid=,ppid=,pgid="], text=True, timeout=5)
    rows = [tuple(map(int, line.split())) for line in output.splitlines()]
    found = {root} | {pid for pid, _, group in rows if group == root}
    while True:
        expanded = found | {pid for pid, parent, _ in rows if parent in found}
        if expanded == found:
            return found
        found = expanded


def swap_bytes():
    value = subprocess.check_output(
        ["sysctl", "-n", "vm.swapusage"], text=True, timeout=5)
    match = re.search(r"used = ([0-9.]+)([KMGT])", value)
    if not match:
        raise RuntimeError("Cannot read system swap; watchdog unavailable")
    return int(float(match[1]) * 1024**("KMGT".index(match[2]) + 1))


def digest(path):
    with open(path, "rb") as source:
        result = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b""):
            result.update(block)
        return result.hexdigest()


def stop(proc, lib, observed):
    # Let time reap the workload and write exit accounting before killing the group.
    for pid in members(proc.pid) - {proc.pid}:
        value = process_usage(lib, pid)
        if value:
            observed[pid] = value["start_ticks"]
    for pid, started in observed.items():
        if pid == proc.pid:
            continue
        value = process_usage(lib, pid)
        if value and value["start_ticks"] == started:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    # Also clean up observed descendants that changed their process group.
    for pid, started in observed.items():
        value = process_usage(lib, pid)
        if value and value["start_ticks"] == started:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    proc.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="New evidence directory")
    parser.add_argument("--scratch", type=Path, required=True, help="Dedicated temporary directory")
    parser.add_argument("--interval", type=float, default=1, help="Sampling seconds (default: 1)")
    parser.add_argument("--footprint-gib", type=float, default=32)
    parser.add_argument("--swap-growth-gib", type=float, default=4)
    parser.add_argument("--min-free-gib", type=float, default=4)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if platform.system() != "Darwin":
        parser.error("This watchdog measures macOS physical footprint")
    if not command or not (binary := shutil.which(command[0])):
        parser.error("Supply -- followed by an existing prebuilt workload executable")
    if not 0.05 <= args.interval <= 60 or not all(0 < value < float("inf") for value in (
            args.footprint_gib, args.swap_growth_gib, args.min_free_gib)):
        parser.error("Use an interval in [0.05, 60] and positive guard thresholds")
    args.scratch.mkdir(parents=True, exist_ok=True)
    args.output.mkdir(parents=True, exist_ok=False)
    lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    lib.proc_pid_rusage.restype = ctypes.c_int
    native = args.output.resolve() / "native-time.txt"
    summary = dict(schema=1, status="starting", command=command,
        executable=str(Path(binary).resolve()), executable_sha256=digest(binary),
        wrapper=TIME, wrapper_sha256=digest(TIME), platform=platform.platform(),
        architecture=platform.machine(), hostname=platform.node(), logical_cpus=os.cpu_count(),
        scratch=str(args.scratch.resolve()),
        native_time=str(native), sample_interval_seconds=args.interval,
        limits=dict(footprint_bytes=args.footprint_gib * GIB,
                    additional_system_swap_bytes=args.swap_growth_gib * GIB,
                    minimum_free_disk_bytes=args.min_free_gib * GIB),
        sampled_peak_tree_footprint_bytes=0, sampled_peak_tree_rss_bytes=0,
        phases=[])
    proc = None
    observed = {}
    start = time.monotonic()
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f"signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        baseline = swap_bytes()
        summary["baseline_system_swap_bytes"] = baseline
        if shutil.disk_usage(args.scratch).free < args.min_free_gib * GIB:
            raise RuntimeError("Insufficient scratch free space before launch")
        env = dict(os.environ, TMPDIR=str(args.scratch.resolve()))
        with (args.output / "command.log").open("wb") as log, \
                (args.output / "command.log").open("rb") as read, \
                (args.output / "samples.jsonl").open("w") as samples:
            proc = subprocess.Popen([TIME, "-l", "-o", str(native), *command],
                stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                env=env, start_new_session=True)
            summary.update(status="running", wrapper_pid=proc.pid)
            pending = b""
            while True:
                tick = time.monotonic()
                values = {}
                for pid in members(proc.pid):
                    value = process_usage(lib, pid)
                    if value:
                        values[pid] = value
                        observed[pid] = value["start_ticks"]
                ended = proc.poll() is not None
                if not values and not ended:
                    raise RuntimeError("No process measurements; watchdog unavailable")
                swap = swap_bytes()
                free = shutil.disk_usage(args.scratch).free
                sample = dict(seconds=tick - start, processes=values,
                    tree_footprint_bytes=sum(v["footprint_bytes"] for v in values.values()),
                    tree_rss_bytes=sum(v["rss_bytes"] for v in values.values()),
                    system_swap_bytes=swap, additional_system_swap_bytes=swap - baseline,
                    scratch_filesystem_free_bytes=free)
                samples.write(json.dumps(sample) + "\n")
                samples.flush()
                for metric in ("tree_footprint_bytes", "tree_rss_bytes"):
                    key = "sampled_peak_" + metric
                    summary[key] = max(summary[key], sample[metric])
                pending += read.read()
                lines = pending.split(b"\n")
                pending = lines.pop()
                if ended and pending:
                    lines.append(pending)
                    pending = b""
                for line in lines:
                    line = line.decode("utf-8", errors="replace").strip()
                    if line.startswith("BENCH "):
                        match = re.search(r"\bseconds=([0-9.]+)", line)
                        summary["phases"].append(dict(marker=line,
                            workload_seconds=float(match[1]) if match else None,
                            observed_seconds=time.monotonic() - start))
                if sample["tree_footprint_bytes"] > args.footprint_gib * GIB:
                    reason = "sampled_tree_footprint"
                elif swap - baseline > args.swap_growth_gib * GIB:
                    reason = "additional_system_swap"
                elif free < args.min_free_gib * GIB:
                    reason = "scratch_free_space"
                else:
                    reason = None
                if reason:
                    summary.update(status="stopped_at_guard", stop_reason=reason)
                    break
                if ended:
                    summary["status"] = "completed" if proc.returncode == 0 else "failed"
                    break
                time.sleep(max(0, args.interval - (time.monotonic() - tick)))
    except KeyboardInterrupt:
        summary["status"] = "interrupted"
    except Exception as error:
        summary.update(status="measurement_error", error=str(error))
    finally:
        if proc:
            try:
                stop(proc, lib, observed)
            except Exception as error:
                summary["cleanup_error"] = str(error)
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                proc.wait()
                summary["status"] = "measurement_error"
            summary["returncode"] = proc.returncode
        summary["wall_seconds_including_cleanup"] = time.monotonic() - start
        (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return 0 if summary["status"] == "completed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
