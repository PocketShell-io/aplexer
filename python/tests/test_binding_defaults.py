"""Signal ABI controls that do not create or signal a runtime session."""

import pytest

from aplexer import _native
from aplexer import client as client_module


def test_public_kill_defaults_to_term_and_preserves_explicit_hup(monkeypatch, tmp_path):
    calls = []

    class Native:
        @staticmethod
        def kill(*args):
            calls.append(args)

    monkeypatch.setattr(client_module, "_native", lambda: Native)
    client = client_module.Client(
        state_dir=tmp_path / "state", runtime_dir=tmp_path / "run",
        config=tmp_path / "config.toml",
    )
    client.kill("not-a-real-session")
    client.kill("not-a-real-session", signal=1)
    assert [args[1] for args in calls] == [15, 1]
    assert all(args[2:] == (2000, *client._path_args()) for args in calls)


def test_native_binding_import_and_signal_validation_without_a_session(tmp_path):
    # Exercise the compiled PyO3 entry point with private paths. Signal 0 is
    # rejected before registry lookup/RPC, so this cannot target a live worker.
    with pytest.raises(RuntimeError, match="signal out of range"):
        _native.kill(
            "not-a-real-session", signal=0,
            state_dir=str(tmp_path / "state"), runtime_dir=str(tmp_path / "run"),
            config=str(tmp_path / "config.toml"),
        )
