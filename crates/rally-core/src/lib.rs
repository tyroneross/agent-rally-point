// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Rust-native Rally coordination kernel.
//!
//! This crate is the greenfield center of gravity for Rally. It owns durable
//! store reads and deterministic projections over Rally events. CLI commands,
//! MCP tools, ACP/A2A bridges, and future sync code should call this crate
//! instead of reinterpreting `changes.jsonl` independently.

pub mod diagnose;
pub mod query;
pub mod store;

use rally_protocol::ProtocolError;
use std::fmt;

#[derive(Debug)]
pub enum CoreError {
    Protocol(ProtocolError),
    InvalidSince(String),
    Io(std::io::Error),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(err) => write!(f, "{err}"),
            Self::InvalidSince(value) => write!(f, "invalid --since value {value:?}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for CoreError {}

impl From<ProtocolError> for CoreError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}

impl From<std::io::Error> for CoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
