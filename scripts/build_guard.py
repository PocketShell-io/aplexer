#!/usr/bin/env python3
"""Preventive compilation growth guard.

Monitors target directory size in real-time during compilation command.
Terminates the entire process group immediately if growth exceeds allowed
threshold or disk free space drops below minimum floor.
Records baseline, peak (inclusive of final measurement), and reports
truthful GUARD_SUCCESS / GUARD_FAIL labels.
"""
import argparse
import os
import signal
import subprocess
import sys
import time


def get_dir_size_mb(path: str) -> int:
    """Measure directory size in MiB using du -sm.
    
    Fails closed (raises RuntimeError) on any du failure or missing output.
    If path does not exist prior to build, returns 0.
    """
    if not os.path.exists(path):
        return 0
    res = subprocess.run(
        ["du", "-sm", path],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if res.returncode != 0:
        raise RuntimeError(
            f"du failed on {path} (exit {res.returncode}): {res.stderr.strip()}"
        )
    tokens = res.stdout.split()
    if not tokens:
        raise RuntimeError(f"du produced empty output for {path}")
    try:
        return int(tokens[0])
    except ValueError as e:
        raise RuntimeError(f"du produced non-integer size '{tokens[0]}': {e}")


def get_free_mb(path: str = "/home") -> int:
    """Measure filesystem free space in MiB for path."""
    try:
        stat = os.statvfs(path)
        return (stat.f_bavail * stat.f_frsize) // (1024 * 1024)
    except Exception as e:
        raise RuntimeError(f"statvfs failed on {path}: {e}")


def terminate_process_group(proc: subprocess.Popen, timeout: float = 2.0):
    """Terminate the child and all descendant processes in its process group."""
    if proc.poll() is not None:
        return
    try:
        pgid = os.getpgid(proc.pid)
    except ProcessLookupError:
        return

    # First attempt: SIGTERM to entire process group
    try:
        os.killpg(pgid, signal.SIGTERM)
    except ProcessLookupError:
        return

    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            return
        time.sleep(0.05)

    # Escalation: SIGKILL to entire process group
    if proc.poll() is None:
        try:
            os.killpg(pgid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=1.0)
        except subprocess.TimeoutExpired:
            pass


def main():
    parser = argparse.ArgumentParser(
        description="Preventive compilation process group and memory growth guard."
    )
    parser.add_argument(
        "--max-growth-mb",
        type=int,
        default=512,
        help="Maximum allowed target directory growth in MiB (default: 512)",
    )
    parser.add_argument(
        "--margin-mb",
        type=int,
        default=16,
        help="Conservative early-stop margin in MiB below hard ceiling (default: 16)",
    )
    parser.add_argument(
        "--min-free-mb",
        type=int,
        default=8192,
        help="Minimum required free space on filesystem in MiB (default: 8192)",
    )
    parser.add_argument(
        "--free-check-path",
        type=str,
        default="/home",
        help="Path for filesystem free space check (default: /home)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=60.0,
        help="Maximum execution duration in seconds (default: 60.0)",
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=0.05,
        help="Polling interval in seconds (default: 0.05)",
    )
    parser.add_argument("target_dir", help="Target build directory to monitor")
    parser.add_argument("command", nargs=argparse.REMAINDER, help="Command to execute")

    args = parser.parse_args()

    # If command starts with '--', strip it
    cmd = args.command
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]

    if not cmd:
        sys.stderr.write("GUARD_ERROR: No command specified to run\n")
        sys.exit(2)

    target_dir = args.target_dir
    max_growth_mb = args.max_growth_mb
    margin_mb = args.margin_mb
    min_free_mb = args.min_free_mb
    free_check_path = args.free_check_path
    timeout_sec = args.timeout
    poll_interval = args.poll_interval

    # Initial filesystem free check (fail closed before launch)
    try:
        free_mb = get_free_mb(free_check_path)
    except Exception as e:
        sys.stderr.write(f"GUARD_ERROR: Failed checking initial free space: {e}\n")
        sys.exit(104)

    if free_mb < min_free_mb:
        sys.stderr.write(
            f"GUARD_ERROR: Disk free space {free_mb} MiB is below minimum floor {min_free_mb} MiB\n"
        )
        sys.exit(102)

    # Initial baseline measurement (fail closed)
    try:
        baseline_mb = get_dir_size_mb(target_dir)
    except Exception as e:
        sys.stderr.write(f"GUARD_ERROR: Failed checking baseline directory size: {e}\n")
        sys.exit(104)

    ceiling_mb = baseline_mb + max_growth_mb
    early_stop_mb = max(baseline_mb, ceiling_mb - margin_mb)

    sys.stderr.write(
        f"GUARD_START: baseline={baseline_mb}M limit={max_growth_mb}M ceiling={ceiling_mb}M "
        f"early_stop={early_stop_mb}M (margin={margin_mb}M) free={free_mb}M timeout={timeout_sec}s\n"
    )

    start_time = time.time()
    # Launch child with start_new_session=True so child becomes process group leader
    try:
        proc = subprocess.Popen(cmd, start_new_session=True)
    except Exception as e:
        sys.stderr.write(f"GUARD_ERROR: Failed to launch command: {e}\n")
        sys.exit(105)

    peak_mb = baseline_mb
    term_reason = None
    exit_code = 0

    def sig_handler(signum, frame):
        terminate_process_group(proc)
        sys.exit(128 + signum)

    signal.signal(signal.SIGINT, sig_handler)
    signal.signal(signal.SIGTERM, sig_handler)

    try:
        while proc.poll() is None:
            time.sleep(poll_interval)
            elapsed = time.time() - start_time

            # 1. Continuous free disk floor check
            try:
                cur_free_mb = get_free_mb(free_check_path)
            except Exception as e:
                term_reason = f"GUARD_TERMINATED: statvfs check failed: {e}"
                exit_code = 104
                break

            if cur_free_mb < min_free_mb:
                term_reason = (
                    f"GUARD_TERMINATED: Free space dropped to {cur_free_mb} MiB "
                    f"(below minimum floor {min_free_mb} MiB)!"
                )
                exit_code = 102
                break

            # 2. Continuous target directory size check (fail closed on du error)
            try:
                cur_mb = get_dir_size_mb(target_dir)
            except Exception as e:
                term_reason = f"GUARD_TERMINATED: du failed during monitoring: {e}"
                exit_code = 104
                break

            if cur_mb > peak_mb:
                peak_mb = cur_mb

            # Early stop check (preventive hard cap enforcement)
            if cur_mb >= early_stop_mb:
                growth = cur_mb - baseline_mb
                term_reason = (
                    f"GUARD_TERMINATED: Target growth {growth} MiB hit conservative ceiling "
                    f"{early_stop_mb - baseline_mb} MiB (cur={cur_mb}M, ceiling={ceiling_mb}M, limit={max_growth_mb}M)!"
                )
                exit_code = 101
                break

            # 3. Timeout check
            if elapsed > timeout_sec:
                term_reason = (
                    f"GUARD_TERMINATED: Compilation timeout exceeded {timeout_sec}s "
                    f"(elapsed {round(elapsed, 2)}s)!"
                )
                exit_code = 103
                break

        if term_reason:
            sys.stderr.write(f"\n{term_reason}\n")
            terminate_process_group(proc)
            proc_code = proc.wait()
            # Preserve failure exit code determined by guard
            final_exit = exit_code
        else:
            final_exit = proc.wait()

    finally:
        if proc.poll() is None:
            terminate_process_group(proc)

    # Final measurement included in peak calculation (fail closed on du error)
    duration = round(time.time() - start_time, 2)
    try:
        final_mb = get_dir_size_mb(target_dir)
        if final_mb > peak_mb:
            peak_mb = final_mb
    except Exception as e:
        sys.stderr.write(f"GUARD_WARNING: Failed final size measurement: {e}\n")
        final_mb = peak_mb

    growth_mb = final_mb - baseline_mb

    if final_exit == 0:
        sys.stderr.write(
            f"GUARD_SUCCESS: baseline={baseline_mb}M peak={peak_mb}M final={final_mb}M "
            f"growth={growth_mb}M duration={duration}s exit=0\n"
        )
    else:
        sys.stderr.write(
            f"GUARD_FAIL: command exited with code {final_exit} (baseline={baseline_mb}M "
            f"peak={peak_mb}M final={final_mb}M growth={growth_mb}M duration={duration}s)\n"
        )

    sys.exit(final_exit)


if __name__ == "__main__":
    main()
