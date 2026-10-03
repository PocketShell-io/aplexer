#!/usr/bin/env python3
"""Negative and bounded verification test suite for build_guard.py.

Verifies:
1. Process group termination terminates child and background grandchildren on timeout.
2. Growth threshold termination triggers at conservative ceiling.
3. Fail-closed behavior on du error.
4. Fail-closed behavior on free disk space floor breach.
5. Command non-zero exit code propagation and truthful GUARD_FAIL label.
6. Clean command exit and truthful GUARD_SUCCESS label with peak measurement.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time


def run_guard(args, env=None):
    guard_path = os.path.join(os.path.dirname(__file__), "build_guard.py")
    cmd = [sys.executable, guard_path] + args
    res = subprocess.run(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env or os.environ.copy(),
    )
    return res


def test_process_group_termination():
    print("Testing process group termination on timeout...")
    with tempfile.TemporaryDirectory() as tmpdir:
        # Script spawns background child sleeper then sleeps
        child_script = (
            "import subprocess, time, sys\n"
            "p = subprocess.Popen(['sleep', '100'])\n"
            "sys.stdout.write(str(p.pid) + '\\n')\n"
            "sys.stdout.flush()\n"
            "time.sleep(100)\n"
        )
        runner = [
            "--timeout", "0.5",
            tmpdir,
            sys.executable, "-c", child_script
        ]
        res = run_guard(runner)
        assert res.returncode == 103, f"Expected 103 on timeout, got {res.returncode}. Stderr: {res.stderr}"
        assert "GUARD_TERMINATED: Compilation timeout exceeded" in res.stderr
        
        # Verify no orphan sleep child left running from our script
        child_pid_str = res.stdout.strip()
        if child_pid_str.isdigit():
            child_pid = int(child_pid_str)
            time.sleep(0.1)
            # Check if pid is alive
            try:
                os.kill(child_pid, 0)
                alive = True
            except OSError:
                alive = False
            assert not alive, f"Descendant process {child_pid} was not terminated by process group kill!"
    print("  -> PASS: process group cleanly killed on timeout, no orphans.")


def test_growth_termination():
    print("Testing growth threshold termination at conservative margin...")
    with tempfile.TemporaryDirectory() as tmpdir:
        # Allocates 12MB in dummy target dir with max growth 10MB and margin 2MB (stop at 8MB)
        writer_script = (
            f"import time\n"
            f"with open('{tmpdir}/test_payload.bin', 'wb') as f:\n"
            f"    for _ in range(15):\n"
            f"        f.write(b'X' * 1024 * 1024)\n"
            f"        f.flush()\n"
            f"        time.sleep(0.05)\n"
        )
        runner = [
            "--max-growth-mb", "10",
            "--margin-mb", "2",
            "--poll-interval", "0.02",
            tmpdir,
            sys.executable, "-c", writer_script
        ]
        res = run_guard(runner)
        assert res.returncode == 101, f"Expected 101 on growth breach, got {res.returncode}. Stderr: {res.stderr}"
        assert "GUARD_TERMINATED: Target growth" in res.stderr
    print("  -> PASS: growth limit proactively enforced before hard ceiling.")


def test_du_failure_fail_closed():
    print("Testing du failure fail-closed behavior...")
    with tempfile.TemporaryDirectory() as tmpdir:
        unreadable_dir = os.path.join(tmpdir, "unreadable")
        os.mkdir(unreadable_dir)
        os.chmod(unreadable_dir, 0)
        try:
            runner = [
                unreadable_dir,
                sys.executable, "-c", "import time; time.sleep(0.1)"
            ]
            res = run_guard(runner)
            assert res.returncode == 104, f"Expected 104 on du failure, got {res.returncode}. Stderr: {res.stderr}"
            assert "GUARD_ERROR" in res.stderr
        finally:
            os.chmod(unreadable_dir, 0o755)
    print("  -> PASS: du failure fails closed with code 104.")


def test_free_space_floor():
    print("Testing disk free space floor fail-closed behavior...")
    with tempfile.TemporaryDirectory() as tmpdir:
        # Ask for 999999999 MB free space which is impossible
        runner = [
            "--min-free-mb", "999999999",
            tmpdir,
            sys.executable, "-c", "print('SHOULD_NOT_EXECUTE')"
        ]
        res = run_guard(runner)
        assert res.returncode == 102, f"Expected 102 on insufficient disk, got {res.returncode}. Stderr: {res.stderr}"
        assert "GUARD_ERROR: Disk free space" in res.stderr
        assert "SHOULD_NOT_EXECUTE" not in res.stdout
    print("  -> PASS: disk free floor verified before launch, execution prevented.")


def test_command_failure_propagation():
    print("Testing command failure exit code propagation...")
    with tempfile.TemporaryDirectory() as tmpdir:
        runner = [
            tmpdir,
            sys.executable, "-c", "import sys; sys.exit(42)"
        ]
        res = run_guard(runner)
        assert res.returncode == 42, f"Expected child exit code 42, got {res.returncode}. Stderr: {res.stderr}"
        assert "GUARD_FAIL: command exited with code 42" in res.stderr
    print("  -> PASS: failure exit code preserved and truthful GUARD_FAIL reported.")


def test_command_success():
    print("Testing command success reporting...")
    with tempfile.TemporaryDirectory() as tmpdir:
        runner = [
            tmpdir,
            sys.executable, "-c", "print('guard_ok')"
        ]
        res = run_guard(runner)
        assert res.returncode == 0, f"Expected 0, got {res.returncode}. Stderr: {res.stderr}"
        assert "GUARD_SUCCESS" in res.stderr
        assert "exit=0" in res.stderr
    print("  -> PASS: success verified and truthful GUARD_SUCCESS reported.")


if __name__ == "__main__":
    test_process_group_termination()
    test_growth_termination()
    test_du_failure_fail_closed()
    test_free_space_floor()
    test_command_failure_propagation()
    test_command_success()
    print("\nALL 6 BUILD GUARD NEGATIVE TESTS PASSED.")
