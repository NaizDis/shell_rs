#![allow(unused_imports, clippy::enum_variant_names)]
use anyhow::{Context, anyhow};
use std::env::{split_paths, var};
use std::fs::Metadata;
use std::os::unix::fs::PermissionsExt;
use std::path;
use std::process::Stdio;
use std::sync::mpsc::Receiver;

pub const BUILT_IN_COMMANDS: [&str; 3] = ["echo", "type", "exit"];
pub enum Command {
    ExitCommand,
    EchoCommand { display_string: exeCmd },
    TypeCommand { command_name: exeCmd },
    ExecCommand { exec_name: exeCmd },
    CommandNotFound,
}
impl Command {
    pub fn from_input(input: &str) -> Self {
        let input = input.trim();
        if input == "exit" {
            return Self::ExitCommand;
        };
        if input.starts_with("echo ") {
            return Command::EchoCommand {
                display_string: exeCmd {
                    name: input[5..].to_string(),
                    path: None,
                    args: vec![],
                },
            };
        }
        if input.starts_with("type ") {
            let parse = if let Ok(parseInput) = Self::parse_input(&input[5..]) {
                parseInput
            } else {
                exeCmd {
                    name: input[5..].to_string(),
                    path: None,
                    args: vec![],
                }
            };
            return Command::TypeCommand {
                command_name: parse,
            };
        }
        if let Ok(data) = Self::parse_input(input) {
            Command::ExecCommand { exec_name: data }
        } else {
            Command::CommandNotFound
        }
    }

    pub fn parse_input(input: &str) -> anyhow::Result<exeCmd> {
        let parts = input.split(' ').collect::<Vec<&str>>();
        if parts.len() < 1 {
            return Err(anyhow!("Invalid Input !!"));
        }

        let cmd_name = parts[0];
        let arggs: Vec<String> = parts[1..]
            .to_owned()
            .iter()
            .map(|s| s.to_owned().to_owned())
            .collect();
        let ptt = if let Ok(path) = Self::get_path_exe(cmd_name) {
            Some(path)
        } else {
            None
        };
        Ok(exeCmd {
            name: cmd_name.to_string(),
            args: arggs,
            path: ptt,
        })
    }

    fn get_path_exe(name: &str) -> anyhow::Result<String> {
        let path_env = var("PATH")?;
        for path in split_paths(&path_env) {
            let full_path = path.join(name);
            if let Ok(meta_data) = std::fs::metadata(&full_path) {
                if meta_data.is_file() && meta_data.permissions().mode() & 0o111 != 0 {
                    return Ok(full_path.to_string_lossy().into_owned());
                }
            }
        }
        Err(anyhow!("Path Not Found"))
    }

    pub fn rn_exec(mdata: &exeCmd) -> anyhow::Result<std::process::ExitStatus> {
        let path = mdata
            .path
            .as_deref()
            .ok_or_else(|| anyhow!("No executable found for path '{}'", mdata.name))?;
        let mut child = std::process::Command::new(path)
            .args(&mdata.args)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .spawn()
            .context("Failed to spawn a process")?;
        child.wait().context("failed to wait on child")
    }
}

pub struct exeCmd {
    pub name: String,
    pub path: Option<String>,
    pub args: Vec<String>,
}

impl exeCmd {
    pub fn new(name: &str, path: Option<String>, args: Vec<String>) -> Self {
        Self {
            name: name.to_owned(),
            path: path,
            args,
        }
    }
}
