#!/usr/bin/env python3
"""Private, explicit per-user service install/update/remove operations."""

from __future__ import annotations

import errno
import fcntl
import os
import plistlib
import re
import secrets
import stat
import subprocess
import sys
import time
import xml.parsers.expat
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable

LINUX_UNIT = "quotabard.service"
MAC_LABEL = "com.aakshintala.quotabard"
COMMAND_TIMEOUT = 10.0
STOP_TIMEOUT = 5.0
STOP_POLL_INTERVAL = 0.05
MAX_MANAGER_OUTPUT = 4 * 1024 * 1024
MAX_DEFINITION_BYTES = 64 * 1024


class ServiceError(Exception):
    """A safe, user-facing service preparation or transaction error."""


@dataclass(frozen=True)
class LinuxState:
    loaded: bool
    active_state: str
    unit_file_state: str
    pid: int

    @property
    def active(self) -> bool:
        return self.active_state == "active"

    @property
    def failed(self) -> bool:
        return self.active_state == "failed"

    @property
    def enabled(self) -> bool:
        return self.unit_file_state in {"enabled", "enabled-runtime"}


@dataclass(frozen=True)
class MacState:
    loaded: bool
    pid: int | None


@dataclass
class MacPIDObservations:
    known: set[int] = field(default_factory=set)

    def inspect(self, uid: int) -> MacState:
        state = mac_inspect(uid)
        if state.loaded and state.pid is not None:
            self.known.add(state.pid)
        return state

    def prove_gone(self) -> None:
        for pid in sorted(self.known):
            _mac_wait_pid_gone(pid)


@dataclass(eq=False)
class RecoveryDirectory:
    parent: DirectoryAnchor
    name: str
    fd: int
    identity: tuple[int, int]
    closed: bool = False

    @property
    def path(self) -> Path:
        return self.parent.path / self.name

    def verify_directory(self) -> None:
        self.parent.verify()
        opened = os.fstat(self.fd)
        named = os.stat(self.name, dir_fd=self.parent.fd, follow_symlinks=False)
        if (
            not stat.S_ISDIR(opened.st_mode)
            or not stat.S_ISDIR(named.st_mode)
            or (opened.st_dev, opened.st_ino) != self.identity
            or (named.st_dev, named.st_ino) != self.identity
            or opened.st_uid != os.geteuid()
            or opened.st_mode & 0o077
        ):
            raise _fail("unsafeRecovery: recovery directory identity changed")

    def close(self) -> None:
        if not self.closed:
            os.close(self.fd)
            self.closed = True


@dataclass(frozen=True)
class FileBackup:
    destination: FileRef
    recovery: RecoveryDirectory
    identity: tuple[int, int, int, int]

    def verify(self) -> None:
        self.recovery.verify_directory()
        actual = _identity_in_directory(self.recovery.fd, "original")
        if actual != self.identity:
            raise _fail("unsafeRecovery: retained backup identity changed")


@dataclass
class FileTransaction:
    backups: list[FileBackup] = field(default_factory=list)
    installed: dict[FileRef, tuple[int, int, int, int]] = field(default_factory=dict)


@dataclass(frozen=True)
class Paths:
    home: Path
    binary: Path
    definition: Path
    binary_parent: Path
    definition_parent: Path


@dataclass(eq=False)
class HomeAnchor:
    path: Path
    components: tuple[str, ...]
    identities: tuple[tuple[int, int], ...]
    fd: int

    @classmethod
    def open(cls, path: Path) -> HomeAnchor:
        components = path.parts[1:]
        flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
        current = os.open("/", flags)
        identities = [(os.fstat(current).st_dev, os.fstat(current).st_ino)]
        try:
            for component in components:
                next_fd = os.open(component, flags, dir_fd=current)
                info = os.fstat(next_fd)
                identities.append((info.st_dev, info.st_ino))
                if current != next_fd:
                    os.close(current)
                current = next_fd
            info = os.fstat(current)
            if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
                raise _fail("HOME must be a private directory owned by the current user")
            return cls(path, tuple(components), tuple(identities), current)
        except BaseException:
            os.close(current)
            raise

    def verify(self) -> None:
        flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
        current = os.open("/", flags)
        try:
            if (os.fstat(current).st_dev, os.fstat(current).st_ino) != self.identities[0]:
                raise _fail("HOME path changed during the service transaction")
            for component, expected in zip(self.components, self.identities[1:]):
                next_fd = os.open(component, flags, dir_fd=current)
                info = os.fstat(next_fd)
                if (info.st_dev, info.st_ino) != expected:
                    os.close(next_fd)
                    raise _fail("HOME path changed during the service transaction")
                os.close(current)
                current = next_fd
        except OSError as error:
            raise _fail("HOME path could not be revalidated safely") from error
        finally:
            os.close(current)
        info = os.fstat(self.fd)
        if (
            (info.st_dev, info.st_ino) != self.identities[-1]
            or not stat.S_ISDIR(info.st_mode)
            or info.st_uid != os.geteuid()
            or info.st_mode & 0o022
        ):
            raise _fail("anchored HOME directory changed unexpectedly")


@dataclass(eq=False)
class DirectoryAnchor:
    home: HomeAnchor
    path: Path
    components: tuple[str, ...]
    identities: tuple[tuple[int, int], ...]
    fd: int

    @classmethod
    def open(cls, home: HomeAnchor, path: Path, *, create: bool) -> DirectoryAnchor | None:
        components = path.relative_to(home.path).parts
        flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
        current = os.dup(home.fd)
        identities: list[tuple[int, int]] = []
        try:
            for component in components:
                try:
                    next_fd = os.open(component, flags, dir_fd=current)
                except FileNotFoundError:
                    if not create:
                        os.close(current)
                        return None
                    try:
                        os.mkdir(component, 0o700, dir_fd=current)
                    except FileExistsError:
                        pass
                    next_fd = os.open(component, flags, dir_fd=current)
                info = os.fstat(next_fd)
                if (
                    not stat.S_ISDIR(info.st_mode)
                    or info.st_uid != os.geteuid()
                    or info.st_mode & 0o022
                ):
                    os.close(next_fd)
                    raise _fail("service directories must be private and owned by the current user")
                identities.append((info.st_dev, info.st_ino))
                os.close(current)
                current = next_fd
            return cls(home, path, tuple(components), tuple(identities), current)
        except BaseException:
            os.close(current)
            raise

    def verify(self) -> None:
        self.home.verify()
        info = os.fstat(self.fd)
        if (
            (info.st_dev, info.st_ino) != self.identities[-1]
            or not stat.S_ISDIR(info.st_mode)
            or info.st_uid != os.geteuid()
            or info.st_mode & 0o022
        ):
            raise _fail("anchored service directory changed unexpectedly")
        flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
        current = os.dup(self.home.fd)
        try:
            for component, expected in zip(self.components, self.identities):
                next_fd = os.open(component, flags, dir_fd=current)
                info = os.fstat(next_fd)
                if (
                    (info.st_dev, info.st_ino) != expected
                    or not stat.S_ISDIR(info.st_mode)
                    or info.st_uid != os.geteuid()
                    or info.st_mode & 0o022
                ):
                    os.close(next_fd)
                    raise _fail("service parent path changed during the transaction")
                os.close(current)
                current = next_fd
        except OSError as error:
            raise _fail("service parent path could not be revalidated safely") from error
        finally:
            os.close(current)


@dataclass(frozen=True)
class FileRef:
    directory: DirectoryAnchor
    name: str
    created_identity: tuple[int, int] | None = field(default=None, compare=False, hash=False)

    @property
    def path(self) -> Path:
        return self.directory.path / self.name


class ServiceTransaction:
    def __init__(self, paths: Paths) -> None:
        self.paths = paths
        self.home = HomeAnchor.open(paths.home)
        self.binary_directory: DirectoryAnchor | None = None
        self.definition_directory: DirectoryAnchor | None = None
        try:
            fcntl.flock(self.home.fd, fcntl.LOCK_EX)
            self.home.verify()
        except BaseException:
            os.close(self.home.fd)
            raise

    def __enter__(self) -> ServiceTransaction:
        return self

    def __exit__(self, *_args) -> None:
        if self.definition_directory is not None:
            os.close(self.definition_directory.fd)
        if self.binary_directory is not None:
            os.close(self.binary_directory.fd)
        os.close(self.home.fd)

    def open_directories(self, *, create: bool) -> None:
        self.binary_directory = DirectoryAnchor.open(
            self.home, self.paths.binary_parent, create=create
        )
        try:
            self.definition_directory = DirectoryAnchor.open(
                self.home, self.paths.definition_parent, create=create
            )
        except BaseException:
            if self.binary_directory is not None:
                os.close(self.binary_directory.fd)
                self.binary_directory = None
            raise

    def verify(self) -> None:
        self.home.verify()
        for directory in (self.binary_directory, self.definition_directory):
            if directory is not None:
                directory.verify()

    def file(self, path: Path) -> FileRef | None:
        directory = (
            self.binary_directory
            if path == self.paths.binary
            else self.definition_directory
            if path == self.paths.definition
            else None
        )
        return FileRef(directory, path.name) if directory is not None else None


def _fail(message: str) -> ServiceError:
    return ServiceError(message)


def _command(argv: list[str]) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            argv,
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="strict",
            timeout=COMMAND_TIMEOUT,
            shell=False,
        )
    except (OSError, subprocess.SubprocessError, UnicodeError) as error:
        raise _fail("could not query or operate the per-user service manager") from error
    if len(result.stdout.encode("utf-8")) > MAX_MANAGER_OUTPUT:
        raise _fail("service-manager output exceeded the safe limit")
    return result


def _successful_output(argv: list[str]) -> str:
    result = _command(argv)
    if result.returncode != 0:
        raise _fail("service-manager query or operation failed")
    return result.stdout


def _run_successful(argv: list[str]) -> None:
    result = _command(argv)
    if result.returncode != 0:
        raise _fail("service-manager operation failed")


def _expect_line(output: str, expected: str) -> None:
    lines = output.splitlines()
    if len(lines) != 1 or lines[0] != expected:
        raise _fail("service-manager returned an unexpected response")


def linux_inspect() -> LinuxState:
    # systemctl show suppresses empty properties unless --all is requested.
    output = _successful_output(
        [
            "systemctl",
            "--user",
            "show",
            "--all",
            "--no-pager",
            "--property=LoadState,ActiveState,MainPID,UnitFileState",
            LINUX_UNIT,
        ]
    )
    values: dict[str, str] = {}
    expected = {"LoadState", "ActiveState", "MainPID", "UnitFileState"}
    for line in output.splitlines():
        key, separator, value = line.partition("=")
        if (
            not separator
            or key not in expected
            or key in values
            or (key != "UnitFileState" and not value)
        ):
            raise _fail("systemd returned an unexpected unit state")
        values[key] = value
    if values.keys() != expected:
        raise _fail("systemd returned an incomplete unit state")

    load = values["LoadState"]
    active = values["ActiveState"]
    file_state = values["UnitFileState"]
    pid_text = values["MainPID"]
    if not re.fullmatch(r"0|[1-9][0-9]{0,9}", pid_text):
        raise _fail("systemd returned an invalid service PID")
    pid = int(pid_text)
    if pid > 2_147_483_647:
        raise _fail("systemd returned an out-of-range service PID")

    if load == "not-found":
        if active != "inactive" or pid != 0 or file_state != "":
            raise _fail("systemd could not establish that the unit is absent")
        return LinuxState(False, "inactive", "", 0)
    if load != "loaded":
        raise _fail("systemd returned an unsupported unit load state")
    if active not in {"active", "inactive", "failed"}:
        raise _fail("systemd returned an unsupported active state")
    if file_state not in {"enabled", "enabled-runtime", "disabled"}:
        raise _fail("systemd returned an unsupported enablement state")
    if (active == "active") != (pid > 0):
        raise _fail("systemd returned an inconsistent service PID")
    return LinuxState(True, active, file_state, pid)


def mac_gui_uid() -> int:
    _expect_line(_successful_output(["launchctl", "managername"]), "Aqua")
    uid_text = _successful_output(["launchctl", "manageruid"]).splitlines()
    if len(uid_text) != 1 or not re.fullmatch(r"0|[1-9][0-9]{0,9}", uid_text[0]):
        raise _fail("launchd returned an invalid manager UID")
    uid = int(uid_text[0])
    if uid != os.geteuid():
        raise _fail("launchd GUI manager does not belong to the current user")
    return uid


def _parse_launchctl_list(output: str) -> dict[str, MacState]:
    lines = output.splitlines()
    if not lines or lines[0].split() != ["PID", "Status", "Label"]:
        raise _fail("launchd list output has an unrecognized header")
    result: dict[str, MacState] = {}
    for line in lines[1:]:
        fields = line.split()
        if len(fields) != 3:
            raise _fail("launchd list output has an unrecognized row")
        pid_text, status_text, label = fields
        if pid_text != "-" and not re.fullmatch(r"[1-9][0-9]{0,9}", pid_text):
            raise _fail("launchd list output has an invalid PID")
        if pid_text != "-" and int(pid_text) > 2_147_483_647:
            raise _fail("launchd list output has an out-of-range PID")
        if not re.fullmatch(r"-?(0|[1-9][0-9]{0,9})", status_text):
            raise _fail("launchd list output has an invalid status")
        if label in result or len(label) > 255 or not re.fullmatch(r"[A-Za-z0-9._-]+", label):
            raise _fail("launchd list output has an invalid or duplicate label")
        result[label] = MacState(True, None if pid_text == "-" else int(pid_text))
    return result


def mac_inspect(_uid: int) -> MacState:
    output = _successful_output(["launchctl", "list"])
    return _parse_launchctl_list(output).get(MAC_LABEL, MacState(False, None))


def _safe_source(path_text: str) -> tuple[int, os.stat_result]:
    path = Path(path_text)
    if not path.is_absolute():
        path = Path.cwd() / path
    try:
        before_path = path.lstat()
    except OSError as error:
        raise _fail("incoming binary is unavailable") from error
    if stat.S_ISLNK(before_path.st_mode) or not stat.S_ISREG(before_path.st_mode):
        raise _fail("incoming binary must be a regular, non-symlink file")
    if before_path.st_uid != os.geteuid():
        raise _fail("incoming binary must belong to the current user")
    if before_path.st_size <= 0 or not (before_path.st_mode & 0o111) or not os.access(path, os.X_OK):
        raise _fail("incoming binary must contain bytes and be executable")
    if before_path.st_mode & 0o022:
        raise _fail("incoming binary must not be group- or world-writable")

    flags = os.O_RDONLY | getattr(os, "O_NONBLOCK", 0)
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
        opened = os.fstat(descriptor)
    except OSError as error:
        raise _fail("incoming binary could not be opened safely") from error
    if (
        not stat.S_ISREG(opened.st_mode)
        or opened.st_dev != before_path.st_dev
        or opened.st_ino != before_path.st_ino
    ):
        os.close(descriptor)
        raise _fail("incoming binary changed during validation")
    if opened.st_size <= 0 or not (opened.st_mode & 0o111) or opened.st_mode & 0o022:
        os.close(descriptor)
        raise _fail("incoming binary permissions are unsafe")
    return descriptor, opened


def _home_paths() -> Paths:
    raw_home = os.environ.get("HOME", "")
    if not raw_home or not os.path.isabs(raw_home):
        raise _fail("HOME must be an absolute directory")
    home = Path(os.path.realpath(raw_home))
    try:
        info = home.stat()
    except OSError as error:
        raise _fail("HOME is not an accessible directory") from error
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid():
        raise _fail("HOME must be a directory owned by the current user")
    if info.st_mode & 0o022:
        raise _fail("HOME must not be group- or world-writable")

    if sys.platform == "linux":
        binary_parent = home / ".local" / "bin"
        binary = binary_parent / "quotabard"
        definition_parent = home / ".config" / "systemd" / "user"
        definition = definition_parent / LINUX_UNIT
    elif sys.platform == "darwin":
        binary_parent = home / "Library" / "Application Support" / "QuotaBar" / "bin"
        binary = binary_parent / "quotabard"
        definition_parent = home / "Library" / "LaunchAgents"
        definition = definition_parent / f"{MAC_LABEL}.plist"
    else:
        raise _fail("service preparation supports only Linux and macOS")
    return Paths(home, binary, definition, binary_parent, definition_parent)


def _existing_regular(path: Path, *, executable: bool = False) -> os.stat_result | None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    except OSError as error:
        raise _fail("could not inspect an installed service file") from error
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise _fail("installed service paths must be regular, non-symlink files")
    if info.st_uid != os.geteuid() or info.st_nlink != 1 or info.st_mode & 0o022:
        raise _fail("installed service file ownership or permissions are unsafe")
    if executable and not (info.st_mode & 0o111):
        raise _fail("installed daemon is not executable")
    return info


def _existing_regular_ref(ref: FileRef, *, executable: bool = False) -> os.stat_result | None:
    ref.directory.verify()
    try:
        info = os.stat(ref.name, dir_fd=ref.directory.fd, follow_symlinks=False)
    except FileNotFoundError:
        return None
    except OSError as error:
        raise _fail("could not inspect an installed service file") from error
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise _fail("installed service paths must be regular, non-symlink files")
    if info.st_uid != os.geteuid() or info.st_nlink != 1 or info.st_mode & 0o022:
        raise _fail("installed service file ownership or permissions are unsafe")
    if executable and not (info.st_mode & 0o111):
        raise _fail("installed daemon is not executable")
    return info


def _read_descriptor(descriptor: int, info: os.stat_result) -> bytes:
    try:
        opened = os.fstat(descriptor)
        if (
            opened.st_dev != info.st_dev
            or opened.st_ino != info.st_ino
            or opened.st_size != info.st_size
            or opened.st_size > MAX_DEFINITION_BYTES
            or opened.st_uid != os.geteuid()
            or opened.st_nlink != 1
            or opened.st_mode & 0o022
            or not stat.S_ISREG(opened.st_mode)
        ):
            raise _fail("installed service file changed during validation")
        chunks: list[bytes] = []
        size = 0
        while True:
            chunk = os.read(descriptor, 65536)
            if not chunk:
                break
            size += len(chunk)
            if size > MAX_DEFINITION_BYTES:
                raise _fail("installed service definition exceeds the safe size limit")
            chunks.append(chunk)
        after = os.fstat(descriptor)
        if (
            after.st_size != opened.st_size
            or after.st_mtime_ns != opened.st_mtime_ns
            or after.st_ctime_ns != opened.st_ctime_ns
        ):
            raise _fail("installed service file changed during validation")
        return b"".join(chunks)
    finally:
        os.close(descriptor)


def _read_regular(path: Path, *, executable: bool = False) -> bytes:
    info = _existing_regular(path, executable=executable)
    if info is None:
        raise _fail("an expected installed service file is missing")
    if info.st_size > MAX_DEFINITION_BYTES:
        raise _fail("installed service definition exceeds the safe size limit")
    flags = os.O_RDONLY | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise _fail("could not open an installed service file safely") from error
    return _read_descriptor(descriptor, info)


def _read_regular_ref(ref: FileRef, *, executable: bool = False) -> bytes:
    info = _existing_regular_ref(ref, executable=executable)
    if info is None:
        raise _fail("an expected installed service file is missing")
    if info.st_size > MAX_DEFINITION_BYTES:
        raise _fail("installed service definition exceeds the safe size limit")
    flags = os.O_RDONLY | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(ref.name, flags, dir_fd=ref.directory.fd)
    except OSError as error:
        raise _fail("could not open an installed service file safely") from error
    return _read_descriptor(descriptor, info)


def _unit_template() -> bytes:
    path = Path(__file__).resolve().parents[2] / "service" / "systemd" / LINUX_UNIT
    data = _read_regular(path)
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise _fail("systemd unit template is not UTF-8") from error
    required = (
        "# Managed by QuotaBar service scripts. Do not edit.\n"
        "\n[Unit]\nDescription=QuotaBar quota daemon\n"
        "\n[Service]\nType=exec\nExecStart=%h/.local/bin/quotabard\n"
        "StandardOutput=journal\nStandardError=journal\n"
        "\n[Install]\nWantedBy=default.target\n"
    )
    if text != required:
        raise _fail("systemd unit template does not match the approved fixed service")
    return data


def _plist_bytes(binary: Path) -> bytes:
    path = Path(__file__).resolve().parents[2] / "service" / "macos" / f"{MAC_LABEL}.plist"
    raw = _read_regular(path)
    try:
        value = plistlib.loads(raw)
    except (plistlib.InvalidFileException, ValueError, TypeError, xml.parsers.expat.ExpatError) as error:
        raise _fail("LaunchAgent template is malformed") from error
    expected_keys = {
        "Label",
        "ProgramArguments",
        "RunAtLoad",
        "StandardOutPath",
        "StandardErrorPath",
    }
    if not isinstance(value, dict) or set(value) != expected_keys:
        raise _fail("LaunchAgent template has unexpected keys")
    if (
        value["Label"] != MAC_LABEL
        or value["ProgramArguments"] != ["__QUOTABAR_EXECUTABLE__"]
        or value["RunAtLoad"] is not True
        or value["StandardOutPath"] != "/dev/null"
        or value["StandardErrorPath"] != "/dev/null"
    ):
        raise _fail("LaunchAgent template does not match the approved fixed service")
    value["ProgramArguments"] = [str(binary)]
    try:
        return plistlib.dumps(value, fmt=plistlib.FMT_XML, sort_keys=False)
    except (TypeError, ValueError) as error:
        raise _fail("could not safely encode LaunchAgent property list") from error


def _managed_files(paths: Paths, transaction: ServiceTransaction) -> tuple[bool, bool]:
    binary_ref = transaction.file(paths.binary)
    definition_ref = transaction.file(paths.definition)
    binary_info = (
        _existing_regular_ref(binary_ref, executable=True) if binary_ref is not None else None
    )
    definition_info = (
        _existing_regular_ref(definition_ref) if definition_ref is not None else None
    )
    if binary_info is None and definition_info is None:
        return False, False
    if binary_ref is None or definition_ref is None or binary_info is None or definition_info is None:
        raise _fail("installation is incomplete; refusing to replace or remove partial files")
    if sys.platform == "linux":
        if _read_regular_ref(definition_ref) != _unit_template():
            raise _fail("existing systemd unit is not the fixed QuotaBar unit")
    else:
        try:
            value = plistlib.loads(_read_regular_ref(definition_ref))
        except (plistlib.InvalidFileException, ValueError, TypeError, xml.parsers.expat.ExpatError) as error:
            raise _fail("existing LaunchAgent is malformed") from error
        if not isinstance(value, dict) or set(value) != {
            "Label",
            "ProgramArguments",
            "RunAtLoad",
            "StandardOutPath",
            "StandardErrorPath",
        }:
            raise _fail("existing LaunchAgent is not the fixed QuotaBar service")
        if (
            value.get("Label") != MAC_LABEL
            or value.get("ProgramArguments") != [str(paths.binary)]
            or value.get("RunAtLoad") is not True
            or value.get("StandardOutPath") != "/dev/null"
            or value.get("StandardErrorPath") != "/dev/null"
        ):
            raise _fail("existing LaunchAgent is not the fixed QuotaBar service")
    return True, True


def _identity_from_stat(info: os.stat_result) -> tuple[int, int, int, int]:
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns


def _identity_ref_anchored(ref: FileRef) -> tuple[int, int, int, int] | None:
    try:
        info = os.stat(ref.name, dir_fd=ref.directory.fd, follow_symlinks=False)
    except FileNotFoundError:
        return None
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise _fail("transaction file was replaced unexpectedly")
    return _identity_from_stat(info)


def _identity_ref(ref: FileRef) -> tuple[int, int, int, int] | None:
    ref.directory.verify()
    return _identity_ref_anchored(ref)


def _sync_directory(directory: DirectoryAnchor) -> None:
    directory.verify()
    os.fsync(directory.fd)


def _new_private_file(
    directory: DirectoryAnchor, prefix: str, mode: int
) -> tuple[FileRef, int, tuple[int, int]]:
    directory.verify()
    flags = (
        os.O_WRONLY
        | os.O_CREAT
        | os.O_EXCL
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_CLOEXEC", 0)
    )
    for _ in range(20):
        ref = FileRef(directory, prefix + secrets.token_hex(16))
        try:
            descriptor = os.open(ref.name, flags, mode, dir_fd=directory.fd)
        except FileExistsError:
            continue
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid():
            os.close(descriptor)
            raise _fail("unsafeRecovery: created staging file has unexpected identity")
        return ref, descriptor, (info.st_dev, info.st_ino)
    raise _fail("could not allocate a unique private staging file")


def _unlink_created(ref: FileRef, expected: tuple[int, int]) -> None:
    identity = _identity_ref(ref)
    if identity is None:
        return
    if identity[:2] != expected:
        raise _fail(f"unsafeRecovery: preserving changed staging artifact at {ref.path}")
    backup = _detach_file(ref, identity)
    _remove_backup(backup)


def _stage_bytes(directory: DirectoryAnchor, content: bytes, mode: int) -> FileRef:
    ref, descriptor, created_identity = _new_private_file(
        directory, ".quotabar-stage-", mode
    )
    owned_descriptor = True
    try:
        os.fchmod(descriptor, mode)
        output = os.fdopen(descriptor, "wb", closefd=True)
        owned_descriptor = False
        with output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        identity = _identity_ref(ref)
        if identity is None or identity[:2] != created_identity:
            raise _fail("unsafeRecovery: staging file identity changed")
        _sync_directory(directory)
        return FileRef(directory, ref.name, created_identity)
    except BaseException:
        if owned_descriptor:
            os.close(descriptor)
        _unlink_created(ref, created_identity)
        raise


def _stage_binary(directory: DirectoryAnchor, source: str) -> FileRef:
    source_fd, before = _safe_source(source)
    ref: FileRef | None = None
    descriptor = -1
    created_identity: tuple[int, int] | None = None
    owned_descriptor = False
    copied = 0
    try:
        ref, descriptor, created_identity = _new_private_file(
            directory, ".quotabar-stage-", 0o700
        )
        owned_descriptor = True
        os.fchmod(descriptor, 0o700)
        output = os.fdopen(descriptor, "wb", closefd=True)
        owned_descriptor = False
        with output:
            while True:
                chunk = os.read(source_fd, 1024 * 1024)
                if not chunk:
                    break
                copied += len(chunk)
                output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
        after = os.fstat(source_fd)
        if (
            copied != before.st_size
            or after.st_size != before.st_size
            or after.st_mtime_ns != before.st_mtime_ns
            or after.st_ctime_ns != before.st_ctime_ns
            or copied == 0
        ):
            raise _fail("incoming binary changed or was incomplete while staging")
        identity = _identity_ref(ref)
        if identity is None or identity[:2] != created_identity:
            raise _fail("unsafeRecovery: staging file identity changed")
        _sync_directory(directory)
        return FileRef(directory, ref.name, created_identity)
    except BaseException:
        if owned_descriptor and descriptor >= 0:
            os.close(descriptor)
        if ref is not None and created_identity is not None:
            _unlink_created(ref, created_identity)
        raise
    finally:
        os.close(source_fd)


def _new_recovery_directory(parent: DirectoryAnchor) -> RecoveryDirectory:
    parent.verify()
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    for _ in range(20):
        name = ".quotabar-recovery-" + secrets.token_hex(16)
        try:
            os.mkdir(name, 0o700, dir_fd=parent.fd)
        except FileExistsError:
            continue
        descriptor = -1
        try:
            descriptor = os.open(name, flags, dir_fd=parent.fd)
            info = os.fstat(descriptor)
            if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid():
                raise _fail("recovery directory has unexpected identity")
            recovery = RecoveryDirectory(
                parent, name, descriptor, (info.st_dev, info.st_ino)
            )
            recovery.verify_directory()
            os.fsync(parent.fd)
            return recovery
        except BaseException as error:
            if descriptor >= 0:
                os.close(descriptor)
            raise _fail(
                f"unsafeRecovery: preserving unverified recovery directory {parent.path / name}"
            ) from error
    raise _fail("could not allocate a unique recovery directory")


def _stat_in_directory(directory_fd: int, name: str) -> os.stat_result | None:
    try:
        return os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
    except FileNotFoundError:
        return None


def _identity_in_directory(
    directory_fd: int, name: str
) -> tuple[int, int, int, int] | None:
    info = _stat_in_directory(directory_fd, name)
    if info is None:
        return None
    if (
        stat.S_ISLNK(info.st_mode)
        or not stat.S_ISREG(info.st_mode)
        or info.st_uid != os.geteuid()
        or info.st_mode & 0o022
    ):
        raise _fail("unsafeRecovery: preserved artifact is not a safe regular file")
    return _identity_from_stat(info)


def _remove_empty_recovery(recovery: RecoveryDirectory) -> None:
    try:
        recovery.verify_directory()
        os.rmdir(recovery.name, dir_fd=recovery.parent.fd)
        _sync_directory(recovery.parent)
    finally:
        recovery.close()


def _stage_cleanup(ref: FileRef) -> None:
    if ref.created_identity is None:
        raise _fail(f"unsafeRecovery: stage identity was not retained for {ref.path}")
    _unlink_created(ref, ref.created_identity)


def _cleanup_stages(stages: Iterable[FileRef]) -> None:
    errors: list[BaseException] = []
    for stage in stages:
        try:
            _stage_cleanup(stage)
        except (OSError, ServiceError) as error:
            errors.append(error)
    if errors:
        raise _fail("unsafeRecovery: staging artifacts were preserved after a failed cleanup") from errors[0]


def _unlink_ref_if_identity(ref: FileRef, expected: tuple[int, int, int, int]) -> None:
    if _identity_ref(ref) != expected:
        raise _fail(f"unsafeRecovery: preserving changed service file at {ref.path}")
    backup = _detach_file(ref, expected)
    _remove_backup(backup)


def _detach_file(ref: FileRef, expected: tuple[int, int, int, int]) -> FileBackup:
    if _identity_ref(ref) != expected:
        raise _fail("installed service file changed before it could be quarantined")
    recovery = _new_recovery_directory(ref.directory)
    moved = False
    try:
        recovery.verify_directory()
        if _identity_in_directory(recovery.fd, "original") is not None:
            raise _fail("unsafeRecovery: recovery destination is already occupied")
        ref.directory.verify()
        os.rename(
            ref.name,
            "original",
            src_dir_fd=ref.directory.fd,
            dst_dir_fd=recovery.fd,
        )
        moved = True
        info = _stat_in_directory(recovery.fd, "original")
        actual = (
            _identity_from_stat(info)
            if info is not None and stat.S_ISREG(info.st_mode)
            else None
        )
        safe = (
            info is not None
            and stat.S_ISREG(info.st_mode)
            and info.st_uid == os.geteuid()
            and not info.st_mode & 0o022
        )
        if actual != expected or not safe:
            if info is not None and (
                stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode)
            ):
                try:
                    os.link(
                        "original",
                        ref.name,
                        src_dir_fd=recovery.fd,
                        dst_dir_fd=ref.directory.fd,
                        follow_symlinks=False,
                    )
                except FileExistsError:
                    pass
                except OSError as error:
                    raise _fail(
                        f"unsafeRecovery: changed destination is preserved at {recovery.path}"
                    ) from error
            raise _fail(
                f"unsafeRecovery: changed destination is preserved at {recovery.path}"
            )
        os.fsync(ref.directory.fd)
        os.fsync(recovery.fd)
        return FileBackup(ref, recovery, actual)
    except BaseException:
        if not moved:
            try:
                _remove_empty_recovery(recovery)
            except BaseException:
                recovery.close()
        else:
            recovery.close()
        raise


def _publish_noclobber(
    destination: FileRef, staged: FileRef, transaction: FileTransaction, paths: ServiceTransaction
) -> None:
    if staged.directory is not destination.directory or staged.created_identity is None:
        raise _fail("staged service file has no matching anchored destination")
    paths.verify()
    stage_identity = _identity_ref(staged)
    if stage_identity is None or stage_identity[:2] != staged.created_identity:
        raise _fail(f"unsafeRecovery: preserving changed staging file at {staged.path}")
    if _identity_ref(destination) is not None:
        raise _fail("service destination appeared during staging; refusing to overwrite it")
    paths.verify()
    try:
        os.link(
            staged.name,
            destination.name,
            src_dir_fd=staged.directory.fd,
            dst_dir_fd=destination.directory.fd,
            follow_symlinks=False,
        )
    except FileExistsError as error:
        raise _fail("service destination appeared during publication; refusing to overwrite it") from error
    identity = _identity_ref_anchored(destination)
    if identity is None or identity[:2] != staged.created_identity:
        raise _fail("unsafeRecovery: published service file identity changed")
    transaction.installed[destination] = identity
    _stage_cleanup(staged)
    paths.verify()
    _sync_directory(destination.directory)


def _commit_files(
    replacements: list[tuple[FileRef, FileRef]],
    expected_existing: dict[FileRef, tuple[int, int, int, int] | None],
    paths: ServiceTransaction,
) -> FileTransaction:
    transaction = FileTransaction()
    try:
        for destination, staged in replacements:
            paths.verify()
            current = _identity_ref(destination)
            expected = expected_existing.get(destination)
            if expected is None:
                if current is not None:
                    raise _fail("service destination appeared during staging; refusing to overwrite it")
            else:
                if current != expected:
                    raise _fail("installed service file changed during staging")
                transaction.backups.append(_detach_file(destination, expected))
                paths.verify()
            _publish_noclobber(destination, staged, transaction, paths)
    except BaseException:
        _rollback_files(transaction)
        raise
    return transaction


def _take_files(
    refs: Iterable[FileRef],
    expected: dict[FileRef, tuple[int, int, int, int] | None],
    paths: ServiceTransaction,
) -> FileTransaction:
    transaction = FileTransaction()
    try:
        for ref in refs:
            paths.verify()
            current = _identity_ref(ref)
            identity = expected.get(ref)
            if identity is None:
                if current is not None:
                    raise _fail("service file appeared during removal; refusing to remove it")
                continue
            if current != identity:
                raise _fail("installed service file changed before removal")
            transaction.backups.append(_detach_file(ref, identity))
            _sync_directory(ref.directory)
    except BaseException:
        _rollback_files(transaction)
        raise
    return transaction


def _remove_backup(backup: FileBackup, *, restored_identity: tuple[int, int, int, int] | None = None) -> None:
    backup.verify()
    if restored_identity is not None and _identity_ref(backup.destination) != restored_identity:
        raise _fail("unsafeRecovery: restored destination identity changed")
    os.unlink("original", dir_fd=backup.recovery.fd)
    os.fsync(backup.recovery.fd)
    backup.recovery.verify_directory()
    os.rmdir(backup.recovery.name, dir_fd=backup.recovery.parent.fd)
    _sync_directory(backup.recovery.parent)
    backup.recovery.close()


def _rollback_files(transaction: FileTransaction) -> None:
    try:
        for ref, identity in transaction.installed.items():
            if _identity_ref(ref) != identity:
                raise _fail("installed file identity changed")
        for backup in transaction.backups:
            backup.verify()
            current = _identity_ref(backup.destination)
            installed = transaction.installed.get(backup.destination)
            if current is not None and current not in {installed, backup.identity}:
                raise _fail("destination was replaced during recovery")
    except (OSError, ServiceError) as error:
        for backup in transaction.backups:
            backup.recovery.close()
        raise _fail("unsafeRecovery: preflight failed; preserved staged and recovery artifacts") from error

    try:
        for ref, identity in reversed(list(transaction.installed.items())):
            _unlink_ref_if_identity(ref, identity)
        for backup in reversed(transaction.backups):
            current = _identity_ref(backup.destination)
            if current is None:
                backup.verify()
                try:
                    os.link(
                        "original",
                        backup.destination.name,
                        src_dir_fd=backup.recovery.fd,
                        dst_dir_fd=backup.destination.directory.fd,
                        follow_symlinks=False,
                    )
                except FileExistsError as error:
                    raise _fail("unsafeRecovery: destination appeared before restore") from error
                if _identity_ref(backup.destination) != backup.identity:
                    raise _fail("unsafeRecovery: restored file identity did not match backup")
            elif current != backup.identity:
                raise _fail("unsafeRecovery: destination changed before restore")
            _remove_backup(backup, restored_identity=backup.identity)
    except (OSError, ServiceError) as error:
        for backup in transaction.backups:
            backup.recovery.close()
        raise _fail("unsafeRecovery: rollback stopped; preserved remaining recovery artifacts") from error


def _finalize_files(transaction: FileTransaction) -> bool:
    try:
        for backup in transaction.backups:
            backup.verify()
    except (OSError, ServiceError):
        for backup in transaction.backups:
            backup.recovery.close()
        return True
    leftovers = False
    for backup in transaction.backups:
        try:
            _remove_backup(backup)
        except (OSError, ServiceError):
            leftovers = True
            backup.recovery.close()
    return leftovers


def _write_generated_plist(staged: FileRef) -> None:
    staged.directory.verify()
    path = staged.path
    result = _command(["plutil", "-lint", str(path)])
    staged.directory.verify()
    if result.returncode != 0:
        raise _fail("temporary LaunchAgent failed native property-list validation")
    if result.stdout.strip() not in {"OK", str(path) + ": OK"}:
        raise _fail("plutil returned an unexpected validation response")


def _linux_reset_failed(state: LinuxState) -> LinuxState:
    if not state.failed:
        return state
    _run_successful(["systemctl", "--user", "reset-failed", LINUX_UNIT])
    reset = linux_inspect()
    if reset.failed or reset.active or reset.pid != 0:
        raise _fail("systemd did not clear the exact failed unit")
    return reset


def _linux_stop() -> LinuxState:
    before = linux_inspect()
    if not before.loaded:
        return before
    if before.failed:
        return _linux_reset_failed(before)
    if not before.active:
        return before
    _run_successful(["systemctl", "--user", "stop", LINUX_UNIT])
    after = linux_inspect()
    if after.active or after.pid != 0:
        raise _fail("systemd did not confirm that the service stopped")
    return _linux_reset_failed(after)


def _mac_wait_pid_gone(pid: int) -> None:
    deadline = time.monotonic() + STOP_TIMEOUT
    while True:
        try:
            os.kill(pid, 0)
        except ProcessLookupError as error:
            if error.errno == errno.ESRCH:
                return
            raise _fail("could not prove the managed process has exited") from error
        except OSError as error:
            raise _fail("could not prove the managed process has exited") from error
        if time.monotonic() >= deadline:
            raise _fail("managed process is still present after service stop")
        time.sleep(STOP_POLL_INTERVAL)


def _mac_stop(uid: int, observations: MacPIDObservations) -> MacState:
    before = observations.inspect(uid)
    if not before.loaded:
        observations.prove_gone()
        return before
    _run_successful(["launchctl", "bootout", f"gui/{uid}/{MAC_LABEL}"])
    after = observations.inspect(uid)
    if after.loaded:
        raise _fail("launchd still lists the service after bootout")
    observations.prove_gone()
    return after


def _initial_state(
    observations: MacPIDObservations, transaction: ServiceTransaction
) -> tuple[str, int | None, LinuxState | MacState]:
    transaction.verify()
    if sys.platform == "linux":
        state = linux_inspect()
        transaction.verify()
        return "linux", None, state
    if sys.platform == "darwin":
        uid = mac_gui_uid()
        state = observations.inspect(uid)
        transaction.verify()
        return "darwin", uid, state
    raise _fail("service preparation supports only Linux and macOS")


def _current_state(
    platform_name: str,
    uid: int | None,
    observations: MacPIDObservations,
    transaction: ServiceTransaction,
) -> LinuxState | MacState:
    transaction.verify()
    state = linux_inspect() if platform_name == "linux" else observations.inspect(_require_uid(uid))
    transaction.verify()
    return state


def _require_uid(uid: int | None) -> int:
    if uid is None:
        raise _fail("current GUI UID was not established")
    return uid


def _stop(
    platform_name: str,
    uid: int | None,
    observations: MacPIDObservations,
    transaction: ServiceTransaction,
) -> LinuxState | MacState:
    transaction.verify()
    state = (
        _linux_stop()
        if platform_name == "linux"
        else _mac_stop(_require_uid(uid), observations)
    )
    transaction.verify()
    return state


def _verify_quiescent(
    platform_name: str,
    uid: int | None,
    observations: MacPIDObservations,
    transaction: ServiceTransaction,
) -> LinuxState | MacState:
    state = _current_state(platform_name, uid, observations, transaction)
    if isinstance(state, LinuxState):
        state = _linux_reset_failed(state)
        if state.active or state.pid != 0:
            raise _fail("systemd did not prove the service inactive before file mutation")
    if isinstance(state, MacState):
        if state.loaded:
            raise _fail("launchd still lists the service before file mutation")
        observations.prove_gone()
    transaction.verify()
    return state


def _start(
    platform_name: str,
    uid: int | None,
    definition: Path,
    observations: MacPIDObservations,
    transaction: ServiceTransaction,
) -> None:
    transaction.verify()
    if platform_name == "linux":
        _run_successful(["systemctl", "--user", "start", LINUX_UNIT])
    else:
        _run_successful(["launchctl", "bootstrap", f"gui/{_require_uid(uid)}", str(definition)])
        if not observations.inspect(_require_uid(uid)).loaded:
            raise _fail("launchd did not list the service after bootstrap")
    transaction.verify()


def _reload_linux(transaction: ServiceTransaction) -> None:
    transaction.verify()
    _run_successful(["systemctl", "--user", "daemon-reload"])
    transaction.verify()


def _cleanup_new_install(
    platform_name: str,
    uid: int | None,
    paths: Paths,
    transaction: FileTransaction,
    observations: MacPIDObservations,
    service_transaction: ServiceTransaction,
) -> None:
    state = _current_state(platform_name, uid, observations, service_transaction)
    if platform_name == "darwin":
        assert isinstance(state, MacState)
        if state.loaded:
            _mac_stop(_require_uid(uid), observations)
        else:
            observations.prove_gone()
        _verify_quiescent(platform_name, uid, observations, service_transaction)
        _rollback_files(transaction)
        return

    assert isinstance(state, LinuxState)
    if state.loaded and (state.active or state.failed):
        state = _stop(platform_name, uid, observations, service_transaction)
    if state.loaded and (state.active or state.failed or state.pid != 0):
        raise _fail("new systemd service could not be stopped safely")
    if state.loaded and state.enabled:
        service_transaction.verify()
        _run_successful(["systemctl", "--user", "disable", LINUX_UNIT])
        service_transaction.verify()

    definition_ref = service_transaction.file(paths.definition)
    binary_ref = service_transaction.file(paths.binary)
    if definition_ref is None or binary_ref is None:
        raise _fail("unsafeRecovery: install directories changed during cleanup")
    expected_definition = transaction.installed.get(definition_ref)
    if expected_definition is None or _identity_ref(definition_ref) != expected_definition:
        raise _fail("unsafeRecovery: new systemd unit changed; preserving recovery files")
    unit_transaction = _take_files(
        [definition_ref], {definition_ref: expected_definition}, service_transaction
    )
    try:
        _reload_linux(service_transaction)
        absent = _current_state(platform_name, uid, observations, service_transaction)
        if absent.loaded or absent.pid != 0 or absent.unit_file_state != "":
            raise _fail("systemd still has the new unit; preserving recovery files")
        expected_binary = transaction.installed.get(binary_ref)
        if expected_binary is None or _identity_ref(binary_ref) != expected_binary:
            raise _fail("unsafeRecovery: new daemon changed; preserving recovery files")
        binary_transaction = _take_files(
            [binary_ref], {binary_ref: expected_binary}, service_transaction
        )
        unit_transaction.backups.extend(binary_transaction.backups)
        if _finalize_files(unit_transaction):
            print("warning: unsafeRecovery left verified install cleanup copies", file=sys.stderr)
    except BaseException:
        _rollback_files(unit_transaction)
        _reload_linux(service_transaction)
        raise


def _restore_previous_state(
    platform_name: str,
    uid: int | None,
    definition: Path,
    previous: LinuxState | MacState,
    observations: MacPIDObservations,
    transaction: ServiceTransaction,
) -> None:
    if platform_name == "linux":
        assert isinstance(previous, LinuxState)
        _reload_linux(transaction)
        if previous.active:
            _start("linux", uid, definition, observations, transaction)
        restored = _current_state(platform_name, uid, observations, transaction)
        if restored.active != previous.active or restored.unit_file_state != previous.unit_file_state:
            raise _fail("systemd could not restore the prior active and enablement state")
    else:
        if isinstance(previous, MacState) and previous.loaded:
            _start("darwin", uid, definition, observations, transaction)
        restored = _current_state(platform_name, uid, observations, transaction)
        if isinstance(previous, MacState) and restored.loaded != previous.loaded:
            raise _fail("launchd could not restore the prior loaded state")


def _rollback_update(
    platform_name: str,
    uid: int | None,
    paths: Paths,
    transaction: FileTransaction,
    previous: LinuxState | MacState,
    observations: MacPIDObservations,
    service_transaction: ServiceTransaction,
) -> None:
    _stop(platform_name, uid, observations, service_transaction)
    _verify_quiescent(platform_name, uid, observations, service_transaction)
    _rollback_files(transaction)
    _restore_previous_state(
        platform_name, uid, paths.definition, previous, observations, service_transaction
    )


def _prepare_staging(
    paths: Paths, source: str, transaction: ServiceTransaction
) -> tuple[FileRef, FileRef]:
    if transaction.binary_directory is None or transaction.definition_directory is None:
        raise _fail("service destination directories were not anchored")
    binary_stage: FileRef | None = None
    definition_stage: FileRef | None = None
    try:
        transaction.verify()
        binary_stage = _stage_binary(transaction.binary_directory, source)
        if sys.platform == "linux":
            definition_data = _unit_template()
            definition_stage = _stage_bytes(
                transaction.definition_directory, definition_data, 0o600
            )
        else:
            definition_data = _plist_bytes(paths.binary)
            definition_stage = _stage_bytes(
                transaction.definition_directory, definition_data, 0o600
            )
            _write_generated_plist(definition_stage)
        transaction.verify()
        return binary_stage, definition_stage
    except BaseException:
        _cleanup_stages(
            staged for staged in (binary_stage, definition_stage) if staged is not None
        )
        raise


def _install(
    source: str, paths: Paths, service_transaction: ServiceTransaction
) -> None:
    if os.geteuid() == 0:
        raise _fail("run service scripts as a non-root user")
    observations = MacPIDObservations()
    platform_name, uid, manager = _initial_state(observations, service_transaction)
    if manager.loaded:
        raise _fail("the fixed QuotaBar service label is already loaded")
    service_transaction.open_directories(create=True)
    binary_ref = service_transaction.file(paths.binary)
    definition_ref = service_transaction.file(paths.definition)
    if binary_ref is None or definition_ref is None:
        raise _fail("service destination directories could not be anchored")
    if _identity_ref(binary_ref) is not None or _identity_ref(definition_ref) is not None:
        raise _fail("service files already exist; use update or remove")

    stages: tuple[FileRef, FileRef] | None = None
    files: FileTransaction | None = None
    warning = False
    try:
        stages = _prepare_staging(paths, source, service_transaction)
        verified = _verify_quiescent(
            platform_name, uid, observations, service_transaction
        )
        if verified.loaded:
            raise _fail("the fixed QuotaBar service appeared during staging")
        files = _commit_files(
            [(binary_ref, stages[0]), (definition_ref, stages[1])],
            {binary_ref: None, definition_ref: None},
            service_transaction,
        )
        if platform_name == "linux":
            _reload_linux(service_transaction)
            service_transaction.verify()
            _run_successful(["systemctl", "--user", "enable", "--now", LINUX_UNIT])
            service_transaction.verify()
        else:
            _start(platform_name, uid, paths.definition, observations, service_transaction)
        warning = _finalize_files(files)
    except BaseException:
        if files is not None:
            try:
                _cleanup_new_install(
                    platform_name,
                    uid,
                    paths,
                    files,
                    observations,
                    service_transaction,
                )
            except BaseException as cleanup_error:
                raise _fail(
                    "install failed; unsafeRecovery preserved files because safe cleanup failed"
                ) from cleanup_error
        raise
    finally:
        if stages is not None:
            _cleanup_stages(stages)
    if warning:
        print("warning: unsafeRecovery left private rollback copies", file=sys.stderr)


def _update(
    source: str, paths: Paths, service_transaction: ServiceTransaction
) -> None:
    if os.geteuid() == 0:
        raise _fail("run service scripts as a non-root user")
    observations = MacPIDObservations()
    platform_name, uid, previous = _initial_state(observations, service_transaction)
    service_transaction.open_directories(create=False)
    present, _ = _managed_files(paths, service_transaction)
    if not present:
        raise _fail("no fixed QuotaBar installation exists to update")
    binary_ref = service_transaction.file(paths.binary)
    definition_ref = service_transaction.file(paths.definition)
    if binary_ref is None or definition_ref is None:
        raise _fail("service destination directories could not be anchored")
    binary_identity = _identity_ref(binary_ref)
    definition_identity = _identity_ref(definition_ref)
    if binary_identity is None or definition_identity is None:
        raise _fail("installed service files changed before update")
    old_identities = {binary_ref: binary_identity, definition_ref: definition_identity}

    stages: tuple[FileRef, FileRef] | None = None
    files: FileTransaction | None = None
    warning = False
    stopped_for_update = False
    try:
        stages = _prepare_staging(paths, source, service_transaction)
        _stop(platform_name, uid, observations, service_transaction)
        _verify_quiescent(platform_name, uid, observations, service_transaction)
        stopped_for_update = True
        files = _commit_files(
            [(binary_ref, stages[0]), (definition_ref, stages[1])],
            old_identities,
            service_transaction,
        )
        if platform_name == "linux":
            _reload_linux(service_transaction)
        if (isinstance(previous, LinuxState) and previous.active) or (
            isinstance(previous, MacState) and previous.loaded
        ):
            _start(
                platform_name, uid, paths.definition, observations, service_transaction
            )
        warning = _finalize_files(files)
    except BaseException:
        if files is not None:
            try:
                _rollback_update(
                    platform_name,
                    uid,
                    paths,
                    files,
                    previous,
                    observations,
                    service_transaction,
                )
            except BaseException as rollback_error:
                raise _fail(
                    "update failed; unsafeRecovery prevented rollback or service restart"
                ) from rollback_error
        elif stopped_for_update:
            try:
                service_transaction.verify()
                if (
                    _identity_ref(binary_ref) != binary_identity
                    or _identity_ref(definition_ref) != definition_identity
                ):
                    raise _fail("installed files changed; leaving service stopped")
                _restore_previous_state(
                    platform_name,
                    uid,
                    paths.definition,
                    previous,
                    observations,
                    service_transaction,
                )
            except BaseException as recovery_error:
                raise _fail(
                    "update failed; unsafeRecovery left the prior service stopped"
                ) from recovery_error
        raise
    finally:
        if stages is not None:
            _cleanup_stages(stages)
    if warning:
        print("warning: unsafeRecovery left private rollback copies", file=sys.stderr)


def _remove(
    platform_name: str,
    uid: int | None,
    paths: Paths,
    previous: LinuxState | MacState,
    observations: MacPIDObservations,
    service_transaction: ServiceTransaction,
) -> None:
    service_transaction.open_directories(create=False)
    present, _ = _managed_files(paths, service_transaction)
    if not present:
        if previous.loaded:
            raise _fail("the fixed service is loaded but its owned files are absent")
        return
    binary_ref = service_transaction.file(paths.binary)
    definition_ref = service_transaction.file(paths.definition)
    if binary_ref is None or definition_ref is None:
        raise _fail("service destination directories could not be anchored")
    binary_identity = _identity_ref(binary_ref)
    definition_identity = _identity_ref(definition_ref)
    if binary_identity is None or definition_identity is None:
        raise _fail("installed service files changed before removal")
    owned_identities = {binary_ref: binary_identity, definition_ref: definition_identity}
    _stop(platform_name, uid, observations, service_transaction)
    _verify_quiescent(platform_name, uid, observations, service_transaction)
    files = FileTransaction()
    disabled = False
    try:
        if platform_name == "linux":
            assert isinstance(previous, LinuxState)
            stopped = _current_state(platform_name, uid, observations, service_transaction)
            assert isinstance(stopped, LinuxState)
            if stopped.active or stopped.pid != 0:
                raise _fail("systemd did not prove the service inactive before removal")
            if previous.enabled:
                disabled = True
                command = ["systemctl", "--user", "disable"]
                if previous.unit_file_state == "enabled-runtime":
                    command.append("--runtime")
                service_transaction.verify()
                _run_successful([*command, LINUX_UNIT])
                service_transaction.verify()
            definition_files = _take_files(
                [definition_ref], {definition_ref: definition_identity}, service_transaction
            )
            files.backups.extend(definition_files.backups)
            _reload_linux(service_transaction)
            absent = _current_state(platform_name, uid, observations, service_transaction)
            assert isinstance(absent, LinuxState)
            if absent.loaded or absent.pid != 0 or absent.unit_file_state != "":
                raise _fail("systemd did not unload the exact unit; binary was preserved")
            binary_files = _take_files(
                [binary_ref], {binary_ref: binary_identity}, service_transaction
            )
            files.backups.extend(binary_files.backups)
        else:
            files = _take_files(
                [definition_ref, binary_ref], owned_identities, service_transaction
            )
        if _finalize_files(files):
            print("warning: unsafeRecovery left removed-file copies", file=sys.stderr)
    except BaseException as failure:
        can_restore_manager = True
        if files.backups:
            try:
                _rollback_files(files)
            except BaseException:
                can_restore_manager = False
        if can_restore_manager:
            try:
                service_transaction.verify()
                if (
                    _identity_ref(binary_ref) != binary_identity
                    or _identity_ref(definition_ref) != definition_identity
                ):
                    raise _fail("service files changed during failed removal")
                if platform_name == "linux":
                    if files.backups:
                        _reload_linux(service_transaction)
                    if disabled and isinstance(previous, LinuxState) and previous.enabled:
                        enable = ["systemctl", "--user", "enable"]
                        if previous.unit_file_state == "enabled-runtime":
                            enable.append("--runtime")
                        service_transaction.verify()
                        _run_successful([*enable, LINUX_UNIT])
                        service_transaction.verify()
                    _restore_previous_state(
                        platform_name,
                        uid,
                        paths.definition,
                        previous,
                        observations,
                        service_transaction,
                    )
                else:
                    _restore_previous_state(
                        platform_name,
                        uid,
                        paths.definition,
                        previous,
                        observations,
                        service_transaction,
                    )
            except BaseException as recovery_error:
                raise _fail(
                    "remove failed; unsafeRecovery left the prior service stopped"
                ) from recovery_error
        else:
            raise _fail(
                "remove failed; unsafeRecovery preserved artifacts and prevented service restart"
            ) from failure
        raise failure


def _help(operation: str | None = None) -> None:
    if operation in {"install", "update"}:
        print(f"Usage: {operation} /path/to/quotabard")
    elif operation == "remove":
        print("Usage: remove")
    else:
        print("Usage: install /path/to/quotabard | update /path/to/quotabard | remove")
    print("Explicit per-user operation only; this script never reads or changes ~/.quotabar.")


def main(argv: list[str]) -> int:
    if sys.version_info < (3, 8):
        print("QuotaBar service scripts require Python 3.8 or newer", file=sys.stderr)
        return 1
    if argv in (["--help"], ["-h"]):
        _help()
        return 0
    if not argv or argv[0] not in {"install", "update", "remove"}:
        _help()
        return 2
    operation = argv[0]
    if operation in {"install", "update"}:
        if len(argv) != 2 or argv[1] in {"-h", "--help"}:
            _help(operation)
            return 0 if len(argv) == 2 else 2
        if argv[1].startswith("-"):
            _help(operation)
            return 2
        try:
            if os.geteuid() == 0:
                raise _fail("run service scripts as a non-root user")
            paths = _home_paths()
            with ServiceTransaction(paths) as service_transaction:
                if operation == "install":
                    _install(argv[1], paths, service_transaction)
                else:
                    _update(argv[1], paths, service_transaction)
        except ServiceError as error:
            print(f"quotabar service {operation} failed: {error}", file=sys.stderr)
            return 1
        except OSError:
            print(f"quotabar service {operation} failed: filesystem operation failed", file=sys.stderr)
            return 1
        print(f"quotabar service {operation} completed; provider quota was not checked")
        return 0
    if len(argv) == 2 and argv[1] in {"-h", "--help"}:
        _help("remove")
        return 0
    if len(argv) != 1:
        _help("remove")
        return 2
    if os.geteuid() == 0:
        print("quotabar service remove failed: run service scripts as a non-root user", file=sys.stderr)
        return 1
    if sys.version_info < (3, 8):
        print("quotabar service remove failed: Python 3.8 or newer is required", file=sys.stderr)
        return 1
    try:
        paths = _home_paths()
        with ServiceTransaction(paths) as service_transaction:
            observations = MacPIDObservations()
            platform_name, uid, previous = _initial_state(observations, service_transaction)
            _remove(
                platform_name,
                uid,
                paths,
                previous,
                observations,
                service_transaction,
            )
    except ServiceError as error:
        print(f"quotabar service remove failed: {error}", file=sys.stderr)
        return 1
    except OSError:
        print("quotabar service remove failed: filesystem operation failed", file=sys.stderr)
        return 1
    print("quotabar service remove completed; ~/.quotabar config and cooldowns were not touched")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
