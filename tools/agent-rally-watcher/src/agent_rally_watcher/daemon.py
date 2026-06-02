# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
# build-loop@tyroneross:canary:agent-rally-watcher
# canary-end
"""Daemon lifecycle: PID file, log rotation, start/stop/status/reload.

Single-instance per channel: PID file at
``~/.agent-rally-watcher/run/<slug>.pid``. Log file at
``~/.agent-rally-watcher/logs/daemon.log`` with size-bounded rotation
(keeps the last 3 segments at 1 MiB each).

This module is intentionally small. The CLI in ``cli.py`` is the operator surface.
"""
from __future__ import annotations

import errno
import logging
import logging.handlers
import os
import signal
import sys
import threading
from dataclasses import dataclass
from pathlib import Path

from .filter import load_consumers
from .watcher import Watcher, run_watcher

DEFAULT_RUN_ROOT = "~/.agent-rally-watcher/run"
DEFAULT_LOG_ROOT = "~/.agent-rally-watcher/logs"
DEFAULT_CONSUMERS_CONFIG = "~/.agent-rally-watcher/consumers.toml"
LOG_BYTES_PER_FILE = 1 * 1024 * 1024  # 1 MiB
LOG_BACKUP_COUNT = 3


@dataclass
class DaemonPaths:
    pid_file: Path
    log_file: Path
    consumers_config: Path

    @classmethod
    def for_slug(cls, slug: str) -> "DaemonPaths":
        run_root = Path(os.path.expanduser(os.environ.get("AGENT_RALLY_WATCHER_RUN_ROOT") or DEFAULT_RUN_ROOT))
        log_root = Path(os.path.expanduser(os.environ.get("AGENT_RALLY_WATCHER_LOG_ROOT") or DEFAULT_LOG_ROOT))
        cfg = Path(os.path.expanduser(os.environ.get("AGENT_RALLY_WATCHER_CONSUMERS") or DEFAULT_CONSUMERS_CONFIG))
        safe = slug.replace("/", "_")
        return cls(
            pid_file=run_root / f"{safe}.pid",
            log_file=log_root / "daemon.log",
            consumers_config=cfg,
        )


def configure_logging(log_file: Path) -> None:
    """Configure root logger with size-bounded rotation."""
    log_file.parent.mkdir(parents=True, exist_ok=True)
    handler = logging.handlers.RotatingFileHandler(
        str(log_file),
        maxBytes=LOG_BYTES_PER_FILE,
        backupCount=LOG_BACKUP_COUNT,
        encoding="utf-8",
    )
    handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s"))
    root = logging.getLogger()
    root.handlers.clear()
    root.addHandler(handler)
    root.setLevel(logging.INFO)


def _read_pid(pid_file: Path) -> int | None:
    try:
        return int(pid_file.read_text(encoding="utf-8").strip())
    except (FileNotFoundError, ValueError, OSError):
        return None


def _is_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except OSError as e:
        return e.errno == errno.EPERM  # process exists but we lack permission


def write_pid(pid_file: Path) -> None:
    pid_file.parent.mkdir(parents=True, exist_ok=True)
    pid_file.write_text(f"{os.getpid()}\n", encoding="utf-8")


def clear_pid(pid_file: Path) -> None:
    try:
        pid_file.unlink()
    except FileNotFoundError:
        pass


def daemon_status(paths: DaemonPaths) -> tuple[str, int | None]:
    """Return ``("running"|"stopped"|"stale", pid_or_None)``."""
    pid = _read_pid(paths.pid_file)
    if pid is None:
        return ("stopped", None)
    if _is_alive(pid):
        return ("running", pid)
    return ("stale", pid)


def start_daemon(
    paths: DaemonPaths,
    channel_dir: Path,
    *,
    foreground: bool = False,
    seek_to_end_on_first_start: bool = True,
) -> int:
    """Start the watcher. Returns 0 on success, non-zero on error.

    ``foreground=True`` blocks in the current process (useful for launchd
    and `agent-rally-watcher run-foreground`). ``False`` forks once and
    returns to caller.

    ``seek_to_end_on_first_start`` (default True / ``--from-now``) seeks
    absent cursors to EOF so only new events dispatch. Pass False
    (``--from-start``) to backfill the entire channel log on first start.
    Existing cursors are honored regardless of this flag.
    """
    status, pid = daemon_status(paths)
    if status == "running":
        print(f"agent-rally-watcher already running (pid={pid})", file=sys.stderr)
        return 1
    if status == "stale":
        clear_pid(paths.pid_file)

    consumers = load_consumers(paths.consumers_config)

    if not foreground:
        # Single fork detach (sufficient when launched via launchd / nohup).
        pid = os.fork()
        if pid > 0:
            # Parent — wait briefly then return
            return 0
        os.setsid()

    configure_logging(paths.log_file)
    write_pid(paths.pid_file)
    stop_event = threading.Event()

    def _term(_signum: int, _frame) -> None:
        stop_event.set()

    signal.signal(signal.SIGTERM, _term)
    signal.signal(signal.SIGINT, _term)

    watcher = Watcher(
        channel_dir=channel_dir,
        consumers=consumers,
        stop_event=stop_event,
        seek_to_end_on_first_start=seek_to_end_on_first_start,
    )
    logging.getLogger(__name__).info(
        "starting: channel=%s consumers=%d log=%s",
        channel_dir,
        len(consumers),
        paths.log_file,
    )
    try:
        run_watcher(watcher)
    except Exception as e:  # noqa: BLE001 — log and exit cleanly
        logging.getLogger(__name__).exception("watcher crashed: %s", e)
        clear_pid(paths.pid_file)
        return 2
    clear_pid(paths.pid_file)
    return 0


def stop_daemon(paths: DaemonPaths, *, timeout: float = 5.0) -> int:
    """Send SIGTERM and wait for the process to exit. Returns 0 on success."""
    import time

    pid = _read_pid(paths.pid_file)
    if pid is None:
        print("agent-rally-watcher: not running (no pid file)", file=sys.stderr)
        return 1
    if not _is_alive(pid):
        clear_pid(paths.pid_file)
        print(f"agent-rally-watcher: stale pid {pid} cleared", file=sys.stderr)
        return 0
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError as e:
        print(f"agent-rally-watcher: kill failed: {e}", file=sys.stderr)
        return 2
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not _is_alive(pid):
            clear_pid(paths.pid_file)
            return 0
        time.sleep(0.1)
    # Last resort: SIGKILL
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass
    clear_pid(paths.pid_file)
    return 0


def reload_daemon(paths: DaemonPaths) -> int:
    """Send SIGHUP — currently a re-exec hint; v0.1 logs a notice only."""
    pid = _read_pid(paths.pid_file)
    if pid is None or not _is_alive(pid):
        print("agent-rally-watcher: not running", file=sys.stderr)
        return 1
    # SIGHUP handler not wired in v0.1; reload by stop+start. Document for users.
    print(
        "agent-rally-watcher: reload via stop+start in v0.1 "
        "(SIGHUP rewire lands in v0.2)",
        file=sys.stderr,
    )
    return 0


# canary: agent-rally-watcher@tyroneross — canonical source: github.com/tyroneross/agent-rally-watcher
