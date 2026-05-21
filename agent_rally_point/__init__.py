"""Agent Rally Point — local-first coordination point for coding agents.

Public API surface. Most users should reach for the CLI (`agent-rally-point ...`)
shipped in v0.2; this Python API is for advanced adapter authors building
integrations into in-process runtimes (LangGraph nodes, CrewAI tools,
AutoGen tool-callable wrappers, etc.).
"""
from __future__ import annotations

__version__ = "0.1.0"

# Channel identity
from .channel_paths import (
    app_slug,
    app_channel_dir,
    ensure_channel_dir,
    apps_root,
    DEFAULT_APPS_ROOT,
)

# Presence (heartbeat)
from .presence import (
    write_presence,
    read_active_presence,
)

# Append-only event log
from .changes import (
    append_change,
    make_record,
)

# Monotonic revision counter
from .revision import bump_revision

# Delta-computing reader
from .checkpoint import checkpoint_read

# Canonical write helper (bumps revision + appends record atomically — use this, not raw append_change)
from .post import post

# Lifecycle hygiene (closeout)
from . import lifecycle

__all__ = [
    "__version__",
    "app_slug",
    "app_channel_dir",
    "ensure_channel_dir",
    "apps_root",
    "DEFAULT_APPS_ROOT",
    "write_presence",
    "read_active_presence",
    "append_change",
    "make_record",
    "bump_revision",
    "checkpoint_read",
    "post",
    "lifecycle",
]
