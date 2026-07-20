#![allow(unused_imports, clippy::enum_variant_names, non_camel_case_types)]
use anyhow::{Context, anyhow};
use std::env::{self, split_paths, var};
use std::fs::Metadata;
use std::os::unix::fs::PermissionsExt;
use std::path::{self, Path};
use std::process::Stdio;
use std::sync::mpsc::Receiver;

pub const BUILT_IN_COMMANDS: [&str; 4] = ["echo", "type", "exit", "pwd"];
pub enum Command {
    ExitCommand,
    PwdCommand,
    CdCommand { directory: exeCmd },
    EchoCommand { display_string: exeCmd },
    TypeCommand { command_name: exeCmd },
    ExecCommand { exec_name: exeCmd },
    Noop,
    CommandNotFound,
}
impl Command {
    //return command type from input
    pub fn from_input(input: &str) -> Self {
        let input = input.trim();
        if input.is_empty() {
            return Command::Noop;
        }
        let parts: Vec<&str> = input.splitn(2, ' ').collect();
        let cmd = parts[0];
        let args = parts.get(1).copied().unwrap_or("");

        match cmd {
            "exit" => Command::ExitCommand,
            "echo" => {
                let args = Self::tokenize(input)[1..].join(" ");
                Command::EchoCommand {
                    display_string: exeCmd {
                        name: args,
                        path: None,
                        args: vec![],
                    },
                }
            }
            "type" => {
                let parse = if let Ok(p) = Command::parse_input(args) {
                    p
                } else {
                    exeCmd {
                        name: args.to_string(),
                        path: None,
                        args: vec![],
                    }
                };
                Command::TypeCommand {
                    command_name: parse,
                }
            }
            "pwd" => Command::PwdCommand,
            "cd" => {
                let parse = if let Ok(p) = Command::parse_input(args) {
                    p
                } else {
                    exeCmd {
                        name: args.to_string(),
                        path: None,
                        args: vec![],
                    }
                };
                Command::CdCommand { directory: parse }
            }
            _ => {
                if let Ok(data) = Command::parse_input(input) {
                    Command::ExecCommand { exec_name: data }
                } else {
                    Command::CommandNotFound
                }
            }
        }
    }

    // parse and validate the input to return coomand executable strcuture
    pub fn parse_input(input: &str) -> anyhow::Result<exeCmd> {
        let parts = Self::tokenize(input);
        if parts.len() < 1 {
            return Err(anyhow!("Invalid Input !!"));
        }

        let cmd_name = &parts[0];
        let arggs: Vec<String> = parts[1..]
            .to_owned()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pta = Self::get_path_exe(cmd_name.as_str())?;
        Ok(exeCmd {
            name: cmd_name.to_string(),
            args: arggs,
            path: Some(pta),
        })
    }

    // Create Tokens/Args/Parse out of given input
    fn tokenize(input: &str) -> Vec<String> {
        let mut tokens = vec![];
        let mut current = String::new();
        let mut in_single = false;
        let mut in_double = false;
        let mut escape_next = false;

        for c in input.chars() {
            if escape_next {
                escape_next = false;
                if in_double {
                    match c {
                        '"' | '\\' | '$' | '`' => current.push(c),
                        '\n' => {}
                        _ => {
                            current.push('\\');
                            current.push(c);
                        }
                    }
                } else {
                    current.push(c);
                }

                continue;
            }
            match c {
                '\\' if !in_single => escape_next = true,
                '\'' if in_single => in_single = false,
                '"' if in_double => in_double = false,
                '\'' if !in_single && !in_double => in_single = true,
                '"' if !in_double && !in_single => in_double = true,
                ' ' if !in_single && !in_double => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(c),
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        tokens
    }

    //current working directory
    pub fn pwd_direc() -> String {
        env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "?/?".to_string())
    }

    // search enviroment varaible for executable path
    fn get_path_exe(name: &str) -> anyhow::Result<String> {
        //if absoulte path executable
        if name.contains('/') {
            let meta = std::fs::metadata(name)?;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Ok(name.to_string());
            }
            return Err(anyhow!("Not executable"));
        }

        //Check in PATH variable
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

    pub fn change_directory(dir: String) -> anyhow::Result<i32> {
        let mut og_pt = dir;
        if og_pt == "~" {
            og_pt = env::home_dir().unwrap().to_string_lossy().into_owned();
        }
        let path = Path::new(&og_pt);
        if env::set_current_dir(path).is_ok() {
            Ok(0)
        } else {
            Err(anyhow!(
                "cd :{} : no such file or directory",
                path.display()
            ))
        }
    }

    // run a command/executable along with args
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
