// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! `cockpitd` entrypoint. The transport/serve loop is wired in chunk C1; for now
//! this prints a hello banner so the workspace builds and runs end-to-end.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!("cockpitd {} starting (transport not yet wired — chunk C1)", cockpitd::VERSION);
}
