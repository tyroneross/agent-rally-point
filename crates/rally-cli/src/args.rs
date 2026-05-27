// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

mod commands;

pub(crate) use commands::*;

use crate::output::CliError;
use rally_core::repo::resolve_channel_dir;
use rally_core::store::ChannelStore;
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct CommonOptions {
    pub(crate) json: bool,
    pub(crate) tool: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) identity_dir: Option<PathBuf>,
    pub(crate) key_id: Option<String>,
    pub(crate) sign: bool,
}

impl CommonOptions {
    fn new() -> Self {
        Self {
            json: false,
            tool: None,
            model: None,
            run_id: None,
            identity_dir: None,
            key_id: None,
            sign: false,
        }
    }

    pub(crate) fn channel_store(&self, command: &'static str) -> Result<ChannelStore, CliError> {
        let cwd = env::current_dir()
            .map_err(|err| CliError::runtime(command, format!("failed to read cwd: {err}")))?;
        let home = env::var_os("HOME").map(PathBuf::from);
        let resolution = resolve_channel_dir(cwd, home.as_deref())
            .map_err(|err| CliError::usage(command, err.to_string()))?;
        Ok(ChannelStore::new(resolution.channel_dir))
    }

    pub(crate) fn tool(&self) -> String {
        self.tool.clone().unwrap_or_else(|| "unknown".to_string())
    }

    pub(crate) fn model(&self) -> String {
        self.model.clone().unwrap_or_else(|| "unknown".to_string())
    }

    pub(crate) fn run_id(&self) -> String {
        self.run_id
            .clone()
            .unwrap_or_else(|| "rally-cli".to_string())
    }

    pub(crate) fn identity_dir(&self) -> Result<PathBuf, CliError> {
        self.identity_dir
            .clone()
            .or_else(default_identity_dir)
            .ok_or_else(|| CliError::usage("identity", "HOME is required for default identity dir"))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WriteArgs {
    pub(crate) command: &'static str,
    pub(crate) common: CommonOptions,
    pub(crate) positional: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
    bools: Vec<String>,
}

impl WriteArgs {
    pub(crate) fn parse(
        command: &'static str,
        args: impl Iterator<Item = String>,
    ) -> Result<Self, CliError> {
        let mut common = CommonOptions::new();
        let mut positional = Vec::new();
        let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut bools = Vec::new();
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--json" => common.json = true,
                "--tool" | "--from-tool" => {
                    let value = take_value(command, &arg, &mut args)?;
                    if arg == "--tool" {
                        common.tool = Some(value);
                    } else {
                        flags.entry(arg).or_default().push(value);
                    }
                }
                "--model" => common.model = Some(take_value(command, &arg, &mut args)?),
                "--run-id" => common.run_id = Some(take_value(command, &arg, &mut args)?),
                "--identity-dir" => {
                    common.identity_dir =
                        Some(PathBuf::from(take_value(command, &arg, &mut args)?));
                }
                "--key-id" => common.key_id = Some(take_value(command, &arg, &mut args)?),
                "--no-ack"
                | "--force"
                | "--strict"
                | "--ids"
                | "--sign"
                | "--start-ping"
                | "--human"
                | "--since-cursor"
                | "--peek"
                | "--from-start"
                | "--auto-claim"
                | "--fail-open"
                | "--no-default-trust-policy" => {
                    if arg == "--sign" {
                        common.sign = true;
                    }
                    bools.push(arg)
                }
                "--files" => {
                    let mut values = Vec::new();
                    while args.peek().is_some_and(|value| !value.starts_with('-')) {
                        if let Some(value) = args.next() {
                            values.push(value);
                        }
                    }
                    flags.insert(arg, values);
                }
                "--to"
                | "--availability"
                | "--branch"
                | "--capability"
                | "--role"
                | "--current-task"
                | "--depends-on"
                | "--artifact"
                | "--artifact-kind"
                | "--uri"
                | "--ref-task"
                | "--owner"
                | "--status"
                | "--verification"
                | "--scope"
                | "--supersedes"
                | "--rationale"
                | "--lesson-kind"
                | "--source-event"
                | "--confidence"
                | "--watch"
                | "--event-kind"
                | "--task"
                | "--subject"
                | "--notes"
                | "--summary"
                | "--reason"
                | "--resource"
                | "--path"
                | "--severity"
                | "--resolution"
                | "--since"
                | "--thread"
                | "--limit"
                | "--stale-after-seconds"
                | "--session-id"
                | "--origin"
                | "--trust-policy"
                | "--kind"
                | "--phase"
                | "--payload"
                | "--thread-id"
                | "--causation-id"
                | "--max-seconds" => {
                    let value = take_value(command, &arg, &mut args)?;
                    flags.entry(arg).or_default().push(value);
                }
                value if value.starts_with('-') => {
                    return Err(CliError::usage(command, format!("unknown option {value}")));
                }
                value => positional.push(value.to_string()),
            }
        }

        Ok(Self {
            command,
            common,
            positional,
            flags,
            bools,
        })
    }

    pub(crate) fn one(&self, flag: &str) -> Option<String> {
        self.flags
            .get(flag)
            .and_then(|values| values.last())
            .cloned()
    }

    pub(crate) fn all(&self, flag: &str) -> Vec<String> {
        self.flags.get(flag).cloned().unwrap_or_default()
    }

    pub(crate) fn required(&self, flag: &str) -> Result<String, CliError> {
        self.one(flag)
            .ok_or_else(|| CliError::usage(self.command, format!("{flag} is required")))
    }

    pub(crate) fn has(&self, flag: &str) -> bool {
        self.bools.iter().any(|value| value == flag)
    }

    pub(crate) fn files(&self) -> Vec<String> {
        self.flags.get("--files").cloned().unwrap_or_default()
    }

    pub(crate) fn identifier(&self) -> Result<String, CliError> {
        match self.positional.as_slice() {
            [value] => Ok(value.clone()),
            [] => Err(CliError::usage(
                self.command,
                format!("{} requires an identifier", self.command),
            )),
            _ => Err(CliError::usage(
                self.command,
                format!("{} accepts one identifier", self.command),
            )),
        }
    }
}

fn take_value(
    command: &'static str,
    flag: &str,
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<String, CliError> {
    args.next()
        .ok_or_else(|| CliError::usage(command, format!("{flag} requires a value")))
}

fn parse_usize(command: &'static str, flag: &str, value: &str) -> Result<usize, CliError> {
    value
        .parse()
        .map_err(|_| CliError::usage(command, format!("{flag} must be a positive integer")))
}

fn parse_i64(command: &'static str, flag: &str, value: &str) -> Result<i64, CliError> {
    value
        .parse()
        .map_err(|_| CliError::usage(command, format!("{flag} must be an integer")))
}

fn default_identity_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".agent-rally-point/identity"))
}
