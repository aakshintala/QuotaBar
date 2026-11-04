from __future__ import annotations

import errno
import json
import os
import plistlib
import stat
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path
from unittest import mock

DAEMON = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(DAEMON / "scripts" / "service"))
import common  # noqa: E402


MOCK_MANAGER = r'''#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

name = Path(sys.argv[0]).name
args = sys.argv[1:]
state_path = Path(os.environ["MOCK_STATE"])
log_path = Path(os.environ["MOCK_LOG"])
state = json.loads(state_path.read_text())

def save():
    state_path.write_text(json.dumps(state))

def log(event):
    try:
        binary = Path(os.environ["MOCK_BINARY"]).read_bytes().decode("latin1")
    except FileNotFoundError:
        binary = None
    record = {
        "tool": name,
        "args": args,
        "event": event,
        "binary": binary,
        "service_state": {
            key: state.get(key)
            for key in ("loaded", "active", "active_state", "enabled", "unit_file_state", "pid")
        },
    }
    with log_path.open("a") as stream:
        stream.write(json.dumps(record) + "\n")

def unexpected():
    state.setdefault("unexpected", []).append([name, args])
    log("unexpected")
    save()
    print("unexpected mocked command", file=sys.stderr)
    raise SystemExit(97)

if name == "systemctl":
    if not args or args[0] != "--user":
        unexpected()
    rest = args[1:]
    if rest == ["show", "--all", "--no-pager", "--property=LoadState,ActiveState,MainPID,UnitFileState", "quotabard.service"]:
        state["show_count"] = state.get("show_count", 0) + 1
        if state.get("fail_show_at") == state["show_count"]:
            log("show-failed")
            save()
            raise SystemExit(4)
        if state.get("malformed_show"):
            print("LoadState=loaded\nActiveState=inactive\nMainPID=0")
            log("show-malformed")
            save()
            raise SystemExit(0)
        if state.get("loaded"):
            load = "loaded"
            active = state.get("active_state") or ("active" if state.get("active") else "inactive")
            pid = state.get("pid", 0)
            file_state = state.get("unit_file_state")
            if file_state is None:
                file_state = "enabled" if state.get("enabled") else "disabled"
        else:
            load, active, pid = "not-found", "inactive", 0
            file_state = state.get("absent_unit_file_state", "")
        print(f"LoadState={load}\nActiveState={active}\nMainPID={pid}\nUnitFileState={file_state}")
        log("show")
        save()
        raise SystemExit(0)
    if rest == ["reset-failed", "quotabard.service"]:
        if state.get("fail_reset_failed_count", 0):
            state["fail_reset_failed_count"] -= 1
            log("reset-failed-error")
            save()
            raise SystemExit(5)
        if state.get("active_state") == "failed":
            state.update(active=False, active_state="inactive", pid=0)
        log("reset-failed")
        save()
        raise SystemExit(0)
    if rest == ["daemon-reload"]:
        state["reload_count"] = state.get("reload_count", 0) + 1
        if state.get("fail_reload_count", 0):
            state["fail_reload_count"] -= 1
            log("daemon-reload-failed")
            save()
            raise SystemExit(5)
        if not Path(os.environ["MOCK_UNIT"]).exists():
            state.update(loaded=False, active=False, active_state="inactive", enabled=False, unit_file_state="", pid=0)
        log("daemon-reload")
        save()
        raise SystemExit(0)
    if rest == ["enable", "--now", "quotabard.service"]:
        if state.get("fail_enable_count", 0):
            state["fail_enable_count"] -= 1
            if state.get("fail_enable_after_mutation"):
                state.update(
                    loaded=True,
                    active=False,
                    active_state="failed",
                    enabled=True,
                    unit_file_state="enabled",
                    pid=0,
                )
            event = "enable-failed-mutated" if state.get("fail_enable_after_mutation") else "enable-failed"
            log(event)
            save()
            raise SystemExit(5)
        state.update(loaded=True, active=True, active_state="active", enabled=True, unit_file_state="enabled", pid=424242)
        log("enable-now")
        save()
        raise SystemExit(0)
    if rest == ["start", "quotabard.service"]:
        if state.get("fail_start_count", 0):
            state["fail_start_count"] -= 1
            log("start-failed")
            save()
            raise SystemExit(5)
        state.update(loaded=True, active=True, active_state="active", pid=424242)
        log("start")
        save()
        raise SystemExit(0)
    if rest == ["stop", "quotabard.service"]:
        if state.get("fail_stop_count", 0):
            state["fail_stop_count"] -= 1
            log("stop-failed")
            save()
            raise SystemExit(5)
        state.update(active=False, active_state="inactive", pid=0)
        log("stop")
        save()
        raise SystemExit(0)
    if rest in (["disable", "quotabard.service"], ["disable", "--runtime", "quotabard.service"]):
        if state.get("fail_disable_count", 0):
            state["fail_disable_count"] -= 1
            log("disable-failed")
            save()
            raise SystemExit(5)
        state.update(enabled=False, unit_file_state="disabled")
        log("disable-runtime" if "--runtime" in rest else "disable")
        save()
        raise SystemExit(0)
    if rest in (["enable", "quotabard.service"], ["enable", "--runtime", "quotabard.service"]):
        runtime = "--runtime" in rest
        state.update(enabled=True, unit_file_state="enabled-runtime" if runtime else "enabled")
        log("enable-runtime" if runtime else "enable")
        save()
        raise SystemExit(0)
    unexpected()

if name == "launchctl":
    if args == ["managername"]:
        log("managername")
        save()
        print(state.get("manager_name", "Aqua"))
        raise SystemExit(0)
    if args == ["manageruid"]:
        log("manageruid")
        save()
        print(state.get("manager_uid", os.geteuid()))
        raise SystemExit(0)
    if args == ["list"]:
        state["list_count"] = state.get("list_count", 0) + 1
        if state.get("fail_list_at") == state["list_count"]:
            log("list-failed")
            save()
            raise SystemExit(4)
        if state.get("malformed_list"):
            log("list-malformed")
            save()
            print(state.get("malformed_list"))
            raise SystemExit(0)
        lines = ["PID\tStatus\tLabel"]
        for row in state.get("other_rows", []):
            lines.append(row)
        script = state.get("list_script", [])
        if state["list_count"] <= len(script):
            snapshot = script[state["list_count"] - 1]
            listed = snapshot.get("loaded", False)
            pid = snapshot.get("pid")
        else:
            listed = state.get("loaded")
            pid = state.get("pid")
        if listed:
            lines.append(f"{pid if pid is not None else '-'}\t0\tcom.aakshintala.quotabard")
        log("list")
        save()
        print("\n".join(lines))
        raise SystemExit(0)
    if len(args) == 2 and args[0] == "bootout":
        if args[1] != f"gui/{os.geteuid()}/com.aakshintala.quotabard":
            unexpected()
        if state.get("fail_bootout_count", 0):
            state["fail_bootout_count"] -= 1
            log("bootout-failed")
            save()
            raise SystemExit(5)
        state["loaded"] = False
        log("bootout")
        save()
        raise SystemExit(0)
    if len(args) == 3 and args[0] == "bootstrap":
        if args[1] != f"gui/{os.geteuid()}" or args[2] != os.environ["MOCK_PLIST"]:
            unexpected()
        if state.get("fail_bootstrap_count", 0):
            state["fail_bootstrap_count"] -= 1
            if state.get("fail_bootstrap_after_load"):
                state.update(loaded=True, pid=state.get("bootstrap_pid", 424243))
            log("bootstrap-failed-loaded" if state.get("fail_bootstrap_after_load") else "bootstrap-failed")
            save()
            raise SystemExit(5)
        state.update(loaded=True, pid=state.get("bootstrap_pid", 424242))
        log("bootstrap")
        save()
        raise SystemExit(0)
    unexpected()

if name == "plutil":
    if args[:1] != ["-lint"] or len(args) != 2:
        unexpected()
    target = Path(args[1])
    try:
        target.relative_to(Path(os.environ["MOCK_HOME"]).resolve())
    except ValueError:
        unexpected()
    state.setdefault("plutil_paths", []).append(str(target))
    log("plutil")
    save()
    if state.get("fail_plutil"):
        print("mock validation failure", file=sys.stderr)
        raise SystemExit(1)
    native = Path("/usr/bin/plutil")
    if os.environ.get("MOCK_USE_NATIVE_PLUTIL") == "1" and native.exists():
        result = __import__("subprocess").run([str(native), "-lint", str(target)], capture_output=True, text=True)
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)
    try:
        import plistlib
        plistlib.loads(target.read_bytes())
    except Exception:
        raise SystemExit(1)
    print(f"{target}: OK")
    raise SystemExit(0)

unexpected()
'''


class ServiceScriptTests(unittest.TestCase):
    def setUp(self) -> None:
        previous = getattr(self, "temp", None)
        if previous is not None:
            previous.cleanup()
        if os.geteuid() == 0:
            self.fail("service-script functional tests must run as a non-root user")
        self.temp = tempfile.TemporaryDirectory(prefix="quotabar-service-tests-")
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "Home & <quoted> ' space"
        self.home.mkdir(mode=0o700)
        self.source = self.root / "incoming quotabard"
        self.marker = self.root / "incoming binary was executed"
        self.set_source(b"first-binary\n")
        self.mockbin = self.root / "mock bin"
        self.mockbin.mkdir()
        self.state_path = self.root / "mock manager.json"
        self.log_path = self.root / "mock manager.jsonl"
        (self.mockbin / "mock-manager").write_text(MOCK_MANAGER)
        os.chmod(self.mockbin / "mock-manager", 0o755)
        for command in ("systemctl", "launchctl", "plutil"):
            (self.mockbin / command).symlink_to(self.mockbin / "mock-manager")
        self.state = {
            "loaded": False,
            "active": False,
            "enabled": False,
            "pid": 0,
            "manager_name": "Aqua",
            "manager_uid": os.geteuid(),
            "other_rows": ["-\t0\tcom.example.unrelated"],
        }
        self.save_state()
        self.env = os.environ.copy()
        self.env.update(
            {
                "HOME": str(self.home),
                "PATH": str(self.mockbin) + os.pathsep + os.environ.get("PATH", os.defpath),
                "MOCK_STATE": str(self.state_path),
                "MOCK_LOG": str(self.log_path),
                "MOCK_HOME": str(self.home),
                "MOCK_USE_NATIVE_PLUTIL": "1",
            }
        )
        (self.home / ".quotabar").mkdir(mode=0o700)
        (self.home / ".quotabar" / "muse").mkdir(mode=0o700)
        (self.home / ".quotabar" / "daemon.json").write_bytes(b"config-sentinel\n")
        (self.home / ".quotabar" / "muse" / "cooldown.json").write_bytes(b"cooldown-sentinel\n")
        self.state_snapshot = self.snapshot_state()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def set_source(self, content: bytes) -> None:
        self.source.write_bytes(
            b"#!/bin/sh\nprintf '%s' ran > '" + str(self.marker).encode() + b"'\n# " + content
        )
        os.chmod(self.source, 0o755)

    def save_state(self) -> None:
        self.state_path.write_text(json.dumps(self.state))

    def load_state(self) -> dict:
        return json.loads(self.state_path.read_text())

    def write_state(self, **changes) -> None:
        self.state = self.load_state()
        if "active" in changes and "active_state" not in changes:
            changes["active_state"] = "active" if changes["active"] else "inactive"
        self.state.update(changes)
        self.save_state()

    def calls(self) -> list[dict]:
        if not self.log_path.exists():
            return []
        return [json.loads(line) for line in self.log_path.read_text().splitlines()]

    def unexpected_calls(self) -> list:
        return self.load_state().get("unexpected", [])

    def paths(self, platform_name: str):
        with mock.patch.object(common.sys, "platform", platform_name), mock.patch.dict(
            os.environ, {"HOME": str(self.home)}
        ):
            return common._home_paths()

    def env_for(self, paths) -> dict:
        env = self.env.copy()
        env.update({"MOCK_BINARY": str(paths.binary), "MOCK_UNIT": str(paths.definition), "MOCK_PLIST": str(paths.definition)})
        return env

    def run_operation(self, platform_name: str, *args: str, patch_kill=None, timeout=None):
        out, err = StringIO(), StringIO()
        env = self.env_for(self.paths(platform_name))
        with mock.patch.object(common.sys, "platform", platform_name), mock.patch.dict(
            os.environ, env
        ), redirect_stdout(out), redirect_stderr(err):
            with mock.patch.object(common, "STOP_TIMEOUT", timeout) if timeout is not None else _null_context():
                if patch_kill is None:
                    result = common.main(list(args))
                else:
                    with mock.patch.object(common.os, "kill", patch_kill):
                        result = common.main(list(args))
        return result, out.getvalue(), err.getvalue()

    def install(self, platform_name: str) -> object:
        result, out, err = self.run_operation(platform_name, "install", str(self.source))
        self.assertEqual((result, err), (0, ""), out + err)
        return self.paths(platform_name)

    def snapshot_state(self) -> dict[str, tuple[bytes | None, int | None, int, int]]:
        root = self.home / ".quotabar"
        result: dict[str, tuple[bytes | None, int | None, int, int]] = {}
        for path in sorted(root.rglob("*")):
            relative = str(path.relative_to(root))
            info = path.stat()
            content = path.read_bytes() if path.is_file() else None
            result[relative] = (
                content,
                stat.S_IMODE(info.st_mode),
                info.st_ino,
                info.st_mtime_ns,
            )
        return result

    def assert_state_unchanged(self) -> None:
        self.assertEqual(self.snapshot_state(), self.state_snapshot)

    def assert_no_executed_binary(self) -> None:
        self.assertFalse(self.marker.exists(), "the incoming daemon must never be executed")

    def assert_no_unexpected_mock_calls(self) -> None:
        self.assertEqual(self.unexpected_calls(), [])

    def make_private_directories(self, path: Path) -> None:
        path.mkdir(parents=True, mode=0o700, exist_ok=True)
        current = path
        while current != self.home:
            os.chmod(current, 0o700)
            current = current.parent

    def seed_unrelated_files(self, paths) -> dict[Path, bytes]:
        self.make_private_directories(paths.binary_parent)
        self.make_private_directories(paths.definition_parent)
        unrelated = paths.binary_parent / "keep-this-file"
        unrelated.write_bytes(b"other executable or file\n")
        other_unit = paths.definition_parent / "unrelated-service"
        other_unit.write_bytes(b"not a QuotaBar service\n")
        return {unrelated: unrelated.read_bytes(), other_unit: other_unit.read_bytes()}

    def command_events(self) -> list[dict]:
        return [event for event in self.calls() if event["tool"] in {"systemctl", "launchctl"}]

    def test_install_update_remove_keep_sentinels_and_other_files_and_never_execute_input(self):
        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name):
                self.setUp()
                try:
                    paths = self.paths(platform_name)
                    other_files = self.seed_unrelated_files(paths)
                    paths = self.install(platform_name)
                    installed = paths.binary.read_bytes()
                    self.assertIn(b"first-binary", installed)
                    self.assertEqual(stat.S_IMODE(paths.binary.stat().st_mode), 0o700)
                    self.assertEqual(self.snapshot_state(), self.state_snapshot)
                    for path, content in other_files.items():
                        self.assertEqual(path.read_bytes(), content)
                    if platform_name == "linux":
                        unit = paths.definition.read_text()
                        self.assertIn("ExecStart=%h/.local/bin/quotabard", unit)
                        self.assertIn("WantedBy=default.target", unit)
                        self.assertIn("StandardOutput=journal", unit)
                        self.assertIn("StandardError=journal", unit)
                    else:
                        plist = plistlib.loads(paths.definition.read_bytes())
                        self.assertEqual(plist["ProgramArguments"], [str(paths.binary)])
                        self.assertEqual(plist["Label"], "com.aakshintala.quotabard")
                        self.assertIs(plist["RunAtLoad"], True)
                        self.assertEqual(plist["StandardOutPath"], "/dev/null")
                        self.assertEqual(plist["StandardErrorPath"], "/dev/null")
                        self.assertIn(b"&amp;", paths.definition.read_bytes())
                        self.assertIn(b"&lt;", paths.definition.read_bytes())

                    self.set_source(b"second-binary\n")
                    pid_probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
                    result, out, err = self.run_operation(
                        platform_name, "update", str(self.source), patch_kill=pid_probe
                    )
                    self.assertEqual((result, err), (0, ""), out + err)
                    updated = paths.binary.read_bytes()
                    self.assertIn(b"second-binary", updated)
                    self.assertNotEqual(installed, updated)
                    if platform_name == "linux":
                        self.assertTrue(self.load_state()["active"])
                    else:
                        self.assertTrue(self.load_state()["loaded"])
                    stop_event = next(
                        event for event in self.command_events()
                        if event["event"] in {"stop", "bootout"}
                    )
                    self.assertEqual(stop_event["binary"], installed.decode("latin1"))
                    start_event = [
                        event for event in self.command_events()
                        if event["event"] in {"start", "bootstrap", "enable-now"}
                    ][-1]
                    self.assertEqual(start_event["binary"], updated.decode("latin1"))

                    result, out, err = self.run_operation(
                        platform_name, "remove", patch_kill=pid_probe
                    )
                    self.assertEqual((result, err), (0, ""), out + err)
                    self.assertFalse(paths.binary.exists())
                    self.assertFalse(paths.definition.exists())
                    self.assertEqual(self.snapshot_state(), self.state_snapshot)
                    for path, content in other_files.items():
                        self.assertEqual(path.read_bytes(), content)
                    self.assert_no_executed_binary()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_first_install_failure_removes_only_new_files_and_preserves_sentinels(self):
        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name):
                self.setUp()
                try:
                    paths = self.paths(platform_name)
                    other_files = self.seed_unrelated_files(paths)
                    if platform_name == "linux":
                        self.write_state(fail_enable_count=1)
                    else:
                        self.write_state(fail_bootstrap_count=1)
                    result, _out, err = self.run_operation(platform_name, "install", str(self.source))
                    self.assertEqual(result, 1, err)
                    self.assertFalse(paths.binary.exists())
                    self.assertFalse(paths.definition.exists())
                    self.assertEqual(self.snapshot_state(), self.state_snapshot)
                    for path, content in other_files.items():
                        self.assertEqual(path.read_bytes(), content)
                    self.assert_no_executed_binary()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_update_start_failure_restores_binary_definition_and_prior_active_state(self):
        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name):
                self.setUp()
                try:
                    paths = self.install(platform_name)
                    old_binary = paths.binary.read_bytes()
                    old_definition = paths.definition.read_bytes()
                    self.set_source(b"new-binary-that-must-roll-back\n")
                    if platform_name == "linux":
                        self.write_state(fail_start_count=1)
                        pid_probe = None
                    else:
                        self.write_state(fail_bootstrap_count=1)
                        pid_probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
                    result, _out, err = self.run_operation(
                        platform_name, "update", str(self.source), patch_kill=pid_probe
                    )
                    self.assertEqual(result, 1, err)
                    self.assertEqual(paths.binary.read_bytes(), old_binary)
                    self.assertEqual(paths.definition.read_bytes(), old_definition)
                    if platform_name == "linux":
                        self.assertTrue(self.load_state()["active"])
                    else:
                        self.assertTrue(self.load_state()["loaded"])
                    self.assert_state_unchanged()
                    self.assert_no_executed_binary()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_update_stop_failure_and_post_stop_query_failure_do_not_replace_binary(self):
        for platform_name in ("linux", "darwin"):
            for failure in ("stop", "query"):
                with self.subTest(platform=platform_name, failure=failure):
                    self.setUp()
                    try:
                        paths = self.install(platform_name)
                        old_binary = paths.binary.read_bytes()
                        self.set_source(b"new-binary-must-not-replace-old\n")
                        if platform_name == "linux":
                            if failure == "stop":
                                self.write_state(fail_stop_count=1)
                            else:
                                # Count only this update: initial query, stop pre-query, post-stop query.
                                self.write_state(show_count=0, fail_show_at=3)
                            pid_probe = None
                        else:
                            if failure == "stop":
                                self.write_state(fail_bootout_count=1)
                            else:
                                # Count only this update: initial list, stop pre-query, post-bootout list.
                                self.write_state(list_count=0, fail_list_at=3)
                            pid_probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
                        result, _out, err = self.run_operation(
                            platform_name, "update", str(self.source), patch_kill=pid_probe
                        )
                        self.assertEqual(result, 1, err)
                        self.assertEqual(paths.binary.read_bytes(), old_binary)
                        self.assert_state_unchanged()
                        self.assert_no_executed_binary()
                        self.assert_no_unexpected_mock_calls()
                    finally:
                        self.tearDown()

    def test_query_failures_and_unknown_manager_formats_never_mean_absence(self):
        cases = [("linux", {"fail_show_at": 1}), ("linux", {"malformed_show": True})]
        cases += [
            ("darwin", {"fail_list_at": 1}),
            ("darwin", {"malformed_list": "not a launchctl list"}),
            ("darwin", {"malformed_list": "PID Status Label\n- 0 com.example.x extra"}),
        ]
        for platform_name, change in cases:
            with self.subTest(platform=platform_name, change=change):
                self.setUp()
                try:
                    paths = self.paths(platform_name)
                    self.write_state(**change)
                    result, _out, err = self.run_operation(platform_name, "install", str(self.source))
                    self.assertEqual(result, 1, err)
                    self.assertFalse(paths.binary.exists())
                    self.assertFalse(paths.definition.exists())
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_launchctl_parser_accepts_only_documented_pid_and_signed_status_columns(self):
        parsed = common._parse_launchctl_list(
            "PID\tStatus\tLabel\n-\t-15\tcom.example.stopped\n71234\t0\tcom.aakshintala.quotabard\n"
        )
        self.assertIsNone(parsed["com.example.stopped"].pid)
        self.assertEqual(parsed["com.aakshintala.quotabard"].pid, 71234)
        for malformed in (
            "PID\tStatus\tLabel\n0\t0\tcom.example.zero\n",
            "PID\tStatus\tLabel\n-\tunknown\tcom.example.bad\n",
            "PID\tStatus\tLabel\n2147483648\t0\tcom.example.bad-pid\n",
            "PID\tStatus\tLabel\n-\t0\tcom.example.ok\n-\t0\tcom.example.ok\n",
        ):
            with self.subTest(output=malformed):
                with self.assertRaises(common.ServiceError):
                    common._parse_launchctl_list(malformed)

    def test_mac_requires_the_current_aqua_manager_and_matching_effective_uid(self):
        for change in ({"manager_name": "Background"}, {"manager_uid": os.geteuid() + 1}):
            with self.subTest(change=change):
                self.setUp()
                try:
                    paths = self.paths("darwin")
                    self.write_state(**change)
                    result, _out, err = self.run_operation("darwin", "install", str(self.source))
                    self.assertEqual(result, 1, err)
                    self.assertFalse(paths.binary.exists())
                    self.assertFalse(paths.definition.exists())
                    events = self.command_events()
                    self.assertFalse(any(event["event"] == "list" for event in events))
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_mac_known_pid_is_probed_when_stop_listing_already_omits_service(self):
        for operation in ("update", "remove"):
            with self.subTest(operation=operation):
                self.setUp()
                try:
                    paths = self.install("darwin")
                    old_binary = paths.binary.read_bytes()
                    self.set_source(b"replacement-binary\n")
                    self.write_state(
                        loaded=True,
                        pid=712399,
                        list_count=0,
                        list_script=[
                            {"loaded": True, "pid": 712399},
                            {"loaded": False},
                            {"loaded": False},
                        ],
                    )
                    probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
                    result, out, err = self.run_operation(
                        "darwin",
                        operation,
                        *([str(self.source)] if operation == "update" else []),
                        patch_kill=probe,
                    )
                    self.assertEqual((result, err), (0, ""), out + err)
                    self.assertEqual(
                        probe.call_args_list,
                        [mock.call(712399, 0), mock.call(712399, 0)],
                    )
                    if operation == "update":
                        self.assertNotEqual(paths.binary.read_bytes(), old_binary)
                    else:
                        self.assertFalse(paths.binary.exists())
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_mac_known_pid_probe_failure_blocks_update_after_listing_disappears(self):
        for probe_result in (PermissionError(errno.EPERM, "denied"), None):
            with self.subTest(probe_result=probe_result):
                self.setUp()
                try:
                    paths = self.install("darwin")
                    old_binary = paths.binary.read_bytes()
                    self.set_source(b"replacement-must-not-publish\n")
                    self.write_state(
                        loaded=True,
                        pid=712401,
                        list_count=0,
                        list_script=[
                            {"loaded": True, "pid": 712401},
                            {"loaded": False},
                            {"loaded": False},
                        ],
                    )
                    bootstrap_count = sum(
                        event["event"] == "bootstrap" for event in self.command_events()
                    )
                    probe = mock.Mock(side_effect=probe_result) if probe_result is not None else mock.Mock(return_value=None)
                    result, _out, err = self.run_operation(
                        "darwin", "update", str(self.source), patch_kill=probe, timeout=0
                    )
                    self.assertEqual(result, 1, err)
                    self.assertEqual(paths.binary.read_bytes(), old_binary)
                    probe.assert_called_once_with(712401, 0)
                    self.assertEqual(
                        sum(event["event"] == "bootstrap" for event in self.command_events()),
                        bootstrap_count,
                    )
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_mac_stop_tracks_every_pid_seen_in_changing_listings(self):
        paths = self.install("darwin")
        self.write_state(
            loaded=True,
            pid=712402,
            list_count=0,
            list_script=[
                {"loaded": True, "pid": 712399},
                {"loaded": True, "pid": 712402},
                {"loaded": False},
                {"loaded": False},
            ],
        )
        probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
        result, out, err = self.run_operation(
            "darwin", "remove", patch_kill=probe
        )
        self.assertEqual((result, err), (0, ""), out + err)
        self.assertEqual(
            probe.call_args_list,
            [
                mock.call(712399, 0),
                mock.call(712402, 0),
                mock.call(712399, 0),
                mock.call(712402, 0),
            ],
        )
        self.assertFalse(paths.binary.exists())
        self.assert_no_unexpected_mock_calls()

    def test_mac_pid_permission_error_and_pid_reuse_block_replacement(self):
        self.install("darwin")
        paths = self.paths("darwin")
        old_binary = paths.binary.read_bytes()
        self.set_source(b"must-not-replace\n")
        self.write_state(loaded=True, pid=712345)
        denied = PermissionError(errno.EPERM, "not allowed")
        probe = mock.Mock(side_effect=denied)
        result, _out, err = self.run_operation("darwin", "update", str(self.source), patch_kill=probe)
        self.assertEqual(result, 1, err)
        self.assertEqual(paths.binary.read_bytes(), old_binary)
        probe.assert_called_once_with(712345, 0)
        self.assert_state_unchanged()

        self.write_state(loaded=True, pid=712346)
        alive = mock.Mock(return_value=None)
        result, _out, err = self.run_operation(
            "darwin", "update", str(self.source), patch_kill=alive, timeout=0
        )
        self.assertEqual(result, 1, err)
        self.assertEqual(paths.binary.read_bytes(), old_binary)
        alive.assert_called_once_with(712346, 0)
        self.assert_state_unchanged()
        self.assert_no_unexpected_mock_calls()

    def test_mac_install_cleanup_keeps_failed_bootstrap_pid_until_absence_is_proved(self):
        paths = self.paths("darwin")
        self.write_state(
            fail_bootstrap_count=1,
            fail_bootstrap_after_load=True,
            bootstrap_pid=712403,
            list_count=0,
            list_script=[
                {"loaded": False},
                {"loaded": False},
                {"loaded": True, "pid": 712403},
                {"loaded": False},
                {"loaded": False},
            ],
        )
        probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
        result, _out, err = self.run_operation(
            "darwin", "install", str(self.source), patch_kill=probe
        )
        self.assertEqual(result, 1, err)
        self.assertEqual(
            probe.call_args_list,
            [mock.call(712403, 0), mock.call(712403, 0)],
        )
        self.assertFalse(paths.binary.exists())
        self.assertFalse(paths.definition.exists())
        self.assert_no_executed_binary()
        self.assert_no_unexpected_mock_calls()

    def test_mac_loaded_but_not_running_service_needs_no_pid_probe(self):
        paths = self.install("darwin")
        old_binary = paths.binary.read_bytes()
        self.write_state(loaded=True, pid=None)
        self.set_source(b"new-loaded-daemon\n")
        probe = mock.Mock(side_effect=AssertionError("no PID exists to probe"))
        result, out, err = self.run_operation(
            "darwin", "update", str(self.source), patch_kill=probe
        )
        self.assertEqual((result, err), (0, ""), out + err)
        probe.assert_not_called()
        self.assertNotEqual(paths.binary.read_bytes(), old_binary)
        self.assertTrue(self.load_state()["loaded"])
        self.assert_no_unexpected_mock_calls()

    def test_update_keeps_a_previously_unloaded_install_unloaded(self):
        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name):
                self.setUp()
                try:
                    paths = self.install(platform_name)
                    if platform_name == "linux":
                        self.write_state(
                            loaded=True,
                            active=False,
                            enabled=False,
                            unit_file_state="disabled",
                            pid=0,
                        )
                    else:
                        self.write_state(loaded=False, pid=None)
                    self.set_source(b"update-while-unloaded\n")
                    probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
                    result, out, err = self.run_operation(
                        platform_name, "update", str(self.source), patch_kill=probe
                    )
                    self.assertEqual((result, err), (0, ""), out + err)
                    state = self.load_state()
                    self.assertFalse(state["active"])
                    if platform_name == "linux":
                        self.assertTrue(state["loaded"])
                        self.assertFalse(state["enabled"])
                    else:
                        self.assertFalse(state["loaded"])
                    self.assert_no_executed_binary()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_existing_unowned_files_and_symlink_paths_are_never_overwritten(self):
        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name):
                self.setUp()
                try:
                    paths = self.paths(platform_name)
                    self.make_private_directories(paths.binary_parent)
                    self.make_private_directories(paths.definition_parent)
                    paths.binary.write_bytes(b"unowned binary\n")
                    paths.definition.write_bytes(b"unowned service definition\n")
                    before = (paths.binary.read_bytes(), paths.definition.read_bytes())
                    result, _out, err = self.run_operation(platform_name, "install", str(self.source))
                    self.assertEqual(result, 1, err)
                    self.assertEqual((paths.binary.read_bytes(), paths.definition.read_bytes()), before)
                    self.assert_state_unchanged()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name, symlink=True):
                self.setUp()
                try:
                    paths = self.paths(platform_name)
                    parent = paths.binary_parent
                    self.make_private_directories(parent.parent)
                    outside = self.root / "outside directory"
                    outside.mkdir()
                    parent.symlink_to(outside, target_is_directory=True)
                    result, _out, err = self.run_operation(platform_name, "install", str(self.source))
                    self.assertEqual(result, 1, err)
                    self.assertFalse((outside / paths.binary.name).exists())
                    self.assert_state_unchanged()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_all_regular_file_readers_open_nonblocking_and_reject_fifo_substitution(self):
        real_open = os.open
        source_flags = []

        def fifo_substitution(path, flags, *args, **kwargs):
            if os.fspath(path) == str(self.source):
                source_flags.append(flags)
                if not flags & getattr(os, "O_NONBLOCK", 0):
                    raise AssertionError("a FIFO substitution would block here")
                raise OSError(errno.EAGAIN, "mock FIFO has no writer")
            return real_open(path, flags, *args, **kwargs)

        with mock.patch.object(common.os, "open", side_effect=fifo_substitution):
            with self.assertRaises(common.ServiceError):
                common._safe_source(str(self.source))
        self.assertEqual(len(source_flags), 1)
        self.assertTrue(source_flags[0] & getattr(os, "O_NONBLOCK", 0))
        self.assertTrue(source_flags[0] & getattr(os, "O_NOFOLLOW", 0))

        template = DAEMON / "service" / "systemd" / "quotabard.service"
        installed = self.root / "installed definition"
        installed.write_bytes(b"regular definition\n")
        os.chmod(installed, 0o600)
        for path in (template, installed):
            with self.subTest(path=path):
                seen = []

                def observed_open(target, flags, *args, **kwargs):
                    if os.fspath(target) == str(path):
                        seen.append(flags)
                        self.assertTrue(flags & getattr(os, "O_NONBLOCK", 0))
                        self.assertTrue(flags & getattr(os, "O_NOFOLLOW", 0))
                    return real_open(target, flags, *args, **kwargs)

                with mock.patch.object(common.os, "open", side_effect=observed_open):
                    self.assertTrue(common._read_regular(path))
                self.assertEqual(len(seen), 1)

    def test_incoming_symlink_is_rejected_and_binary_is_never_executed(self):
        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name):
                self.setUp()
                try:
                    link = self.root / "incoming symlink"
                    link.symlink_to(self.source)
                    result, _out, err = self.run_operation(platform_name, "install", str(link))
                    self.assertEqual(result, 1, err)
                    self.assert_no_executed_binary()
                    self.assert_state_unchanged()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_malformed_plist_template_and_plutil_failure_happen_before_bootout(self):
        self.install("darwin")
        paths = self.paths("darwin")
        old_binary = paths.binary.read_bytes()
        old_definition = paths.definition.read_bytes()
        sandbox = self.root / "template sandbox" / "daemon"
        fake_module = sandbox / "scripts" / "service" / "common.py"
        fake_module.parent.mkdir(parents=True)
        fake_module.write_text("# test path anchor\n")
        fake_plist = sandbox / "service" / "macos" / "com.aakshintala.quotabard.plist"
        fake_plist.parent.mkdir(parents=True)
        fake_plist.write_text("<plist><broken>")
        self.set_source(b"new-binary\n")
        with mock.patch.object(common, "__file__", str(fake_module)):
            result, _out, err = self.run_operation("darwin", "update", str(self.source))
        self.assertEqual(result, 1, err)
        self.assertEqual(paths.binary.read_bytes(), old_binary)
        self.assertEqual(paths.definition.read_bytes(), old_definition)
        self.assertFalse(any(event["event"] == "bootout" for event in self.command_events()))

        self.write_state(fail_plutil=True)
        result, _out, err = self.run_operation("darwin", "update", str(self.source))
        self.assertEqual(result, 1, err)
        self.assertEqual(paths.binary.read_bytes(), old_binary)
        self.assertFalse(any(event["event"] == "bootout" for event in self.command_events()))
        self.assert_state_unchanged()
        self.assert_no_unexpected_mock_calls()

    def test_remove_preserves_configuration_and_unrelated_home_files(self):
        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name):
                self.setUp()
                try:
                    paths = self.install(platform_name)
                    unrelated = paths.definition_parent / "other-user-service"
                    unrelated.write_bytes(b"leave me\n")
                    pid_probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
                    result, out, err = self.run_operation(
                        platform_name, "remove", patch_kill=pid_probe
                    )
                    self.assertEqual((result, err), (0, ""), out + err)
                    self.assertFalse(paths.binary.exists())
                    self.assertFalse(paths.definition.exists())
                    self.assertEqual(unrelated.read_bytes(), b"leave me\n")
                    self.assert_state_unchanged()
                    self.assert_no_executed_binary()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_failed_linux_removal_restores_runtime_enablement_and_active_state(self):
        paths = self.install("linux")
        old_binary = paths.binary.read_bytes()
        old_definition = paths.definition.read_bytes()
        self.write_state(unit_file_state="enabled-runtime", fail_reload_count=1)

        result, _out, err = self.run_operation("linux", "remove")

        self.assertEqual(result, 1, err)
        self.assertEqual(paths.binary.read_bytes(), old_binary)
        self.assertEqual(paths.definition.read_bytes(), old_definition)
        state = self.load_state()
        self.assertTrue(state["active"])
        self.assertEqual(state["unit_file_state"], "enabled-runtime")
        events = self.command_events()
        self.assertTrue(any(event["event"] == "disable-runtime" for event in events))
        self.assertTrue(any(event["event"] == "enable-runtime" for event in events))
        self.assert_state_unchanged()
        self.assert_no_unexpected_mock_calls()

    def test_linux_failed_zero_pid_unit_can_be_removed_without_starting_it(self):
        paths = self.install("linux")
        unrelated = paths.definition_parent / "keep-another-unit"
        unrelated.write_bytes(b"other unit\n")
        self.write_state(active=False, active_state="failed", pid=0)

        result, out, err = self.run_operation("linux", "remove")

        self.assertEqual((result, err), (0, ""), out + err)
        self.assertFalse(paths.binary.exists())
        self.assertFalse(paths.definition.exists())
        self.assertEqual(unrelated.read_bytes(), b"other unit\n")
        state = self.load_state()
        self.assertFalse(state["loaded"])
        self.assertFalse(state["active"])
        self.assertFalse(state["enabled"])
        events = [event["event"] for event in self.command_events()]
        self.assertIn("reset-failed", events)
        self.assertNotIn("start", events)
        self.assert_state_unchanged()
        self.assert_no_unexpected_mock_calls()

    def test_linux_failed_zero_pid_update_clears_failure_without_restarting_inactive_unit(self):
        paths = self.install("linux")
        self.set_source(b"updated failed service\n")
        starts_before = sum(event["event"] == "start" for event in self.command_events())
        self.write_state(active=False, active_state="failed", pid=0)

        result, out, err = self.run_operation("linux", "update", str(self.source))

        self.assertEqual((result, err), (0, ""), out + err)
        self.assertIn(b"updated failed service", paths.binary.read_bytes())
        state = self.load_state()
        self.assertTrue(state["loaded"])
        self.assertFalse(state["active"])
        self.assertTrue(state["enabled"])
        self.assertEqual(
            sum(event["event"] == "start" for event in self.command_events()), starts_before
        )
        self.assertIn("reset-failed", [event["event"] for event in self.command_events()])
        self.assert_state_unchanged()
        self.assert_no_unexpected_mock_calls()

    def test_partial_linux_enable_now_failure_is_disabled_unloaded_and_removed(self):
        paths = self.paths("linux")
        self.write_state(fail_enable_count=1, fail_enable_after_mutation=True)

        result, _out, err = self.run_operation("linux", "install", str(self.source))

        self.assertEqual(result, 1, err)
        self.assertFalse(paths.binary.exists())
        self.assertFalse(paths.definition.exists())
        state = self.load_state()
        self.assertFalse(state["loaded"])
        self.assertFalse(state["active"])
        self.assertFalse(state["enabled"])
        events = [event["event"] for event in self.command_events()]
        self.assertIn("enable-failed-mutated", events)
        self.assertIn("reset-failed", events)
        self.assertIn("disable", events)
        self.assertFalse(any(event in {"start", "enable-now"} for event in events))
        self.assert_state_unchanged()
        self.assert_no_executed_binary()
        self.assert_no_unexpected_mock_calls()

    def test_linux_failed_state_with_nonzero_pid_is_rejected_without_mutation(self):
        paths = self.install("linux")
        old_binary = paths.binary.read_bytes()
        self.write_state(active=False, active_state="failed", pid=712404)

        result, _out, err = self.run_operation("linux", "remove")

        self.assertEqual(result, 1, err)
        self.assertEqual(paths.binary.read_bytes(), old_binary)
        self.assertTrue(paths.definition.exists())
        self.assertFalse(any(event["event"] == "reset-failed" for event in self.command_events()))
        self.assert_no_unexpected_mock_calls()

    def test_failed_mac_file_removal_restores_the_loaded_agent(self):
        paths = self.install("darwin")
        old_binary = paths.binary.read_bytes()
        old_definition = paths.definition.read_bytes()
        real_rename = os.rename
        failed = False

        def fail_binary_move(source, destination, *args, **kwargs):
            nonlocal failed
            if not failed and source == paths.binary.name and destination == "original":
                failed = True
                raise OSError(errno.EACCES, "mocked file move failure")
            return real_rename(source, destination, *args, **kwargs)

        with mock.patch.object(common.os, "rename", side_effect=fail_binary_move):
            result, _out, err = self.run_operation(
                "darwin",
                "remove",
                patch_kill=mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone")),
            )

        self.assertEqual(result, 1, err)
        self.assertTrue(failed)
        self.assertEqual(paths.binary.read_bytes(), old_binary)
        self.assertEqual(paths.definition.read_bytes(), old_definition)
        self.assertFalse(list(paths.binary_parent.glob(".quotabar-*")))
        self.assertFalse(list(paths.definition_parent.glob(".quotabar-*")))
        self.assertTrue(self.load_state()["loaded"])
        self.assertTrue(any(event["event"] == "bootstrap" for event in self.command_events()))
        self.assert_state_unchanged()
        self.assert_no_unexpected_mock_calls()

    def test_linux_native_absence_allows_first_install(self):
        paths = self.paths("linux")
        result, out, err = self.run_operation("linux", "install", str(self.source))
        self.assertEqual((result, err), (0, ""), out + err)
        self.assertTrue(paths.binary.is_file())
        self.assertTrue(paths.definition.is_file())
        self.assert_no_unexpected_mock_calls()

    def test_unloaded_removal_is_idempotent_but_failed_absence_query_is_not(self):
        for platform_name in ("linux", "darwin"):
            with self.subTest(platform=platform_name):
                self.setUp()
                try:
                    result, out, err = self.run_operation(platform_name, "remove")
                    self.assertEqual((result, err), (0, ""), out + err)
                    result, out, err = self.run_operation(platform_name, "remove")
                    self.assertEqual((result, err), (0, ""), out + err)
                    self.assert_state_unchanged()
                    self.assert_no_unexpected_mock_calls()
                finally:
                    self.tearDown()

    def test_manager_stop_is_exact_and_command_order_proves_quiescence_before_replace(self):
        paths = self.install("darwin")
        old_binary = paths.binary.read_bytes()
        self.set_source(b"a later binary\n")
        self.write_state(loaded=True, pid=712399)
        pid_probe = mock.Mock(side_effect=ProcessLookupError(errno.ESRCH, "gone"))
        result, out, err = self.run_operation(
            "darwin", "update", str(self.source), patch_kill=pid_probe
        )
        self.assertEqual((result, err), (0, ""), out + err)
        events = self.command_events()
        bootout = next(index for index, event in enumerate(events) if event["event"] == "bootout")
        bootstrap = max(index for index, event in enumerate(events) if event["event"] == "bootstrap")
        self.assertEqual(events[bootout]["args"], ["bootout", f"gui/{os.geteuid()}/com.aakshintala.quotabard"])
        self.assertLess(bootout, bootstrap)
        absent_checks = [event for event in events[bootout + 1 : bootstrap] if event["event"] == "list"]
        self.assertGreaterEqual(len(absent_checks), 2)
        self.assertTrue(all(not event["service_state"]["loaded"] for event in absent_checks))
        self.assertTrue(all(event["binary"] == old_binary.decode("latin1") for event in absent_checks))
        self.assertEqual(
            pid_probe.call_args_list,
            [mock.call(712399, 0), mock.call(712399, 0)],
        )
        self.assert_no_unexpected_mock_calls()

    def test_group_or_world_writable_home_is_rejected_before_manager_query(self):
        paths = self.paths("linux")
        env = self.env_for(paths)
        os.chmod(self.home, 0o777)
        out, err = StringIO(), StringIO()
        try:
            with mock.patch.object(common.sys, "platform", "linux"), mock.patch.dict(
                os.environ, env
            ), redirect_stdout(out), redirect_stderr(err):
                result = common.main(["install", str(self.source)])
        finally:
            os.chmod(self.home, 0o700)
        self.assertEqual(result, 1)
        self.assertIn("HOME must not be group- or world-writable", err.getvalue())
        self.assertEqual(self.command_events(), [])
        self.assert_no_unexpected_mock_calls()

    def test_non_root_boundary_and_incoming_bytes_executable_permissions(self):
        paths = self.paths("darwin")
        env = self.env_for(paths)
        out, err = StringIO(), StringIO()
        with mock.patch.object(common.sys, "platform", "darwin"), mock.patch.dict(
            os.environ, env
        ), mock.patch.object(common.os, "geteuid", return_value=0), redirect_stdout(
            out
        ), redirect_stderr(err):
            result = common.main(["install", str(self.source)])
        self.assertEqual(result, 1)
        self.assertIn("non-root", err.getvalue())
        self.assertEqual(self.command_events(), [])

        for content, mode in ((b"bytes but not executable", 0o644), (b"", 0o755), (b"writable", 0o777)):
            with self.subTest(mode=oct(mode), size=len(content)):
                self.source.write_bytes(content)
                os.chmod(self.source, mode)
                result, _out, err = self.run_operation("darwin", "install", str(self.source))
                self.assertEqual(result, 1, err)
                self.assertFalse(self.paths("darwin").binary.exists())
                self.assertFalse(self.paths("darwin").definition.exists())
                self.assert_state_unchanged()
                self.assert_no_unexpected_mock_calls()

    def test_shell_entrypoints_help_and_python_prerequisite(self):
        scripts = DAEMON / "scripts" / "service"
        for command in ("install", "update", "remove"):
            result = subprocess.run(
                [str(scripts / command), "--help"],
                check=False,
                capture_output=True,
                text=True,
                env=self.env,
                shell=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Usage:", result.stdout)

        result = subprocess.run(
            [str(scripts / "install"), str(self.source)],
            check=False,
            capture_output=True,
            text=True,
            env={"HOME": str(self.home), "PATH": str(self.mockbin)},
            shell=False,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("require Python 3", result.stderr)
        self.assertFalse(self.paths("darwin").binary.exists())

    def test_home_lock_serializes_service_transactions(self):
        paths = self.paths("linux")
        second_home = common.HomeAnchor.open(paths.home)
        try:
            with common.ServiceTransaction(paths):
                with self.assertRaises(BlockingIOError):
                    common.fcntl.flock(
                        second_home.fd, common.fcntl.LOCK_EX | common.fcntl.LOCK_NB
                    )
            common.fcntl.flock(
                second_home.fd, common.fcntl.LOCK_EX | common.fcntl.LOCK_NB
            )
        finally:
            os.close(second_home.fd)

    def test_first_install_destination_appearance_is_never_overwritten(self):
        paths = self.paths("linux")
        unexpected = b"concurrent unowned destination\n"
        real_link = os.link
        injected = False

        def appear_before_publish(source, destination, *args, **kwargs):
            nonlocal injected
            if not injected and destination == paths.binary.name:
                injected = True
                fd = os.open(
                    destination,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL,
                    0o600,
                    dir_fd=kwargs["dst_dir_fd"],
                )
                try:
                    os.write(fd, unexpected)
                finally:
                    os.close(fd)
            return real_link(source, destination, *args, **kwargs)

        with mock.patch.object(common.os, "link", side_effect=appear_before_publish):
            result, _out, err = self.run_operation("linux", "install", str(self.source))

        self.assertEqual(result, 1, err)
        self.assertTrue(injected)
        self.assertEqual(paths.binary.read_bytes(), unexpected)
        self.assertFalse(paths.definition.exists())
        self.assertFalse(self.load_state()["loaded"])
        self.assertFalse(
            any(event["event"] in {"enable-now", "bootstrap"} for event in self.command_events())
        )
        self.assertFalse(list(paths.binary_parent.glob(".quotabar-stage-*")))
        self.assert_no_unexpected_mock_calls()

    def test_destination_substitution_is_restored_and_prevents_update_restart(self):
        paths = self.install("linux")
        old_binary = paths.binary.read_bytes()
        preserved_original = paths.binary.with_name("unexpected-original")
        unexpected = b"concurrent replacement\n"
        self.set_source(b"candidate must not replace the race\n")
        real_rename = os.rename
        injected = False

        def substitute_before_detach(source, destination, *args, **kwargs):
            nonlocal injected
            if not injected and source == paths.binary.name and destination == "original":
                injected = True
                paths.binary.rename(preserved_original)
                paths.binary.write_bytes(unexpected)
            return real_rename(source, destination, *args, **kwargs)

        with mock.patch.object(common.os, "rename", side_effect=substitute_before_detach):
            result, _out, err = self.run_operation("linux", "update", str(self.source))

        self.assertEqual(result, 1, err)
        self.assertTrue(injected)
        self.assertEqual(paths.binary.read_bytes(), unexpected)
        self.assertEqual(preserved_original.read_bytes(), old_binary)
        self.assertFalse(self.load_state()["active"])
        events = [event["event"] for event in self.command_events()]
        last_stop = max(index for index, event in enumerate(events) if event == "stop")
        self.assertNotIn("start", events[last_stop + 1 :])
        self.assert_no_unexpected_mock_calls()

    def test_parent_substitution_keeps_outside_file_and_prevents_update_restart(self):
        paths = self.install("linux")
        old_binary = paths.binary.read_bytes()
        self.set_source(b"candidate must stay in the anchored directory\n")
        anchored_parent = paths.binary_parent.with_name("bin-anchored")
        outside = self.home / "outside"
        outside.mkdir(mode=0o700)
        outside_binary = outside / paths.binary.name
        outside_binary.write_bytes(b"outside sentinel\n")
        real_link = os.link
        injected = False

        def substitute_parent_before_publish(source, destination, *args, **kwargs):
            nonlocal injected
            if not injected and destination == paths.binary.name:
                injected = True
                paths.binary_parent.rename(anchored_parent)
                paths.binary_parent.symlink_to(outside, target_is_directory=True)
            return real_link(source, destination, *args, **kwargs)

        with mock.patch.object(common.os, "link", side_effect=substitute_parent_before_publish):
            result, _out, err = self.run_operation("linux", "update", str(self.source))

        self.assertEqual(result, 1, err)
        self.assertTrue(injected)
        self.assertEqual(outside_binary.read_bytes(), b"outside sentinel\n")
        self.assertEqual(
            next(anchored_parent.glob(".quotabar-recovery-*/original")).read_bytes(),
            old_binary,
        )
        self.assertFalse(self.load_state()["active"])
        events = [event["event"] for event in self.command_events()]
        last_stop = max(index for index, event in enumerate(events) if event == "stop")
        self.assertNotIn("start", events[last_stop + 1 :])
        self.assert_no_unexpected_mock_calls()

    def test_recovery_file_substitution_fails_closed_without_restart(self):
        paths = self.install("linux")
        old_binary = paths.binary.read_bytes()
        self.set_source(b"candidate after backup substitution\n")
        real_link = os.link
        injected = False
        substituted = b"foreign recovery replacement\n"

        def replace_backup_before_definition_publish(source, destination, *args, **kwargs):
            nonlocal injected
            if not injected and destination == paths.definition.name:
                injected = True
                recovery = next(paths.binary_parent.glob(".quotabar-recovery-*"))
                original = recovery / "original"
                original.rename(recovery / "original-preserved")
                original.write_bytes(substituted)
                raise OSError(errno.EIO, "mocked publication failure after recovery substitution")
            return real_link(source, destination, *args, **kwargs)

        with mock.patch.object(
            common.os, "link", side_effect=replace_backup_before_definition_publish
        ):
            result, _out, err = self.run_operation("linux", "update", str(self.source))

        self.assertEqual(result, 1, err)
        self.assertIn("unsafeRecovery", err)
        self.assertTrue(injected)
        recovery = next(paths.binary_parent.glob(".quotabar-recovery-*"))
        self.assertEqual((recovery / "original-preserved").read_bytes(), old_binary)
        self.assertEqual((recovery / "original").read_bytes(), substituted)
        self.assertFalse(self.load_state()["active"])
        events = [event["event"] for event in self.command_events()]
        last_stop = max(index for index, event in enumerate(events) if event == "stop")
        self.assertNotIn("start", events[last_stop + 1 :])
        self.assert_no_unexpected_mock_calls()


class NullContext:
    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False


def _null_context():
    return NullContext()


if __name__ == "__main__":
    unittest.main()
