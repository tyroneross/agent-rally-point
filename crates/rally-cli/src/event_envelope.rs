// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Compatibility import for the shared causal delivery context.
//!
//! R0 moved the provider-neutral envelope and validation vocabulary into
//! `rally-protocol`. Keeping this private module preserves every existing CLI
//! call site while preventing a second envelope model from drifting here.

pub(crate) use rally_protocol::delivery::{CompatMode, EventEnvelope, ProtocolEventKind};
