# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Run ``python -m agent_rally_point`` as a stable, warning-free entry point.

Dispatches to the discover CLI (same as the ``agent-rally-discover`` console
script). Provided so users have one canonical ``-m`` form that avoids the
runpy/submodule-in-sys.modules edge case.

To run other subcommands as modules: ``python -m agent_rally_point.migrate ...``.
"""
from __future__ import annotations

import sys

from agent_rally_point.discover import _main

if __name__ == "__main__":
    sys.exit(_main())
