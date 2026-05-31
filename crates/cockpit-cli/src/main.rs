// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `cockpit-cli` — the headless phone stand-in used to verify the daemon
//! end-to-end without a physical device. Subcommands (list/open/send/approve/
//! launch) are wired in chunk C2; this skeleton keeps the workspace building.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    eprintln!(
        "cockpit-cli: subcommands wired in chunk C2 (args seen: {:?})",
        args
    );
}
