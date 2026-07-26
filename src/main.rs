#![allow(unused_imports, clippy::enum_variant_names)]
use std::char::ToUppercase;
use std::env::{self, split_paths, var};
use std::fs::Metadata;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;

pub mod cmd;
use cmd::{BUILT_IN_COMMANDS, Command};

use crate::cmd::CompleteAction;

fn main() {
    loop {
        //command
        let input = Command::read_input_with_completion();
        let command = Command::from_input(&input);

        match command {
            Command::ExitCommand => break,
            Command::PwdCommand { stdout_file } => {
                let output = Command::pwd_direc();
                if let Some(ref file) = stdout_file {
                    let _ = std::fs::write(file, &output);
                } else {
                    println!("{}", output);
                }
            }
            Command::CdCommand { directory } => {
                if let Err(e) = Command::change_directory(directory.name) {
                    eprintln!("{}", e)
                }
            }
            Command::EchoCommand { display_string } => {
                if let Some(ref file) = display_string.stdout_file {
                    let _ = std::fs::write(file, display_string.name);
                } else {
                    println!("{}", display_string.name);
                }
            }
            Command::TypeCommand { command_name } => {
                let output = if BUILT_IN_COMMANDS.contains(&command_name.name.as_str()) {
                    format!("{} is a shell builtin", command_name.name)
                } else if let Some(ref path) = command_name.path {
                    format!("{} is {}", command_name.name, path)
                } else {
                    format!("{} not found", command_name.name)
                };
                if let Some(ref file) = command_name.stdout_file {
                    let _ = std::fs::write(file, &output);
                } else {
                    println!("{}", output);
                }
            }
            Command::CompleteCommand {
                subcommnad: action,
                stdout_file,
                stderr_file,
            } => match action {
                CompleteAction::Register { script, command } => {
                    Command::completions()
                        .lock()
                        .unwrap()
                        .insert(command, script);
                }
                CompleteAction::Print { command } => {
                    let map = Command::completions().lock().unwrap();
                    let output = if let Some(script) = map.get(&command) {
                        format!("complete -C '{}' {}", script, command)
                    } else {
                        format!("complete : {}: no complete specifications", command)
                    };
                    if let Some(ref file) = stdout_file {
                        let _ = std::fs::write(file, &output);
                    } else {
                        println!("{}", output);
                    }
                }
                CompleteAction::Remove { command } => {
                    Command::completions().lock().unwrap().remove(&command);
                }
                CompleteAction::Error(msg) => eprintln!("{}", msg),
                CompleteAction::Empty => {}
            },
            Command::ExecCommand { exec_name } => {
                if let Err(e) = Command::rn_exec(&exec_name) {
                    eprintln!("{}", e);
                }
            }
            Command::Noop => {}
            Command::CommandNotFound => println!("{} command not found!!", input.trim()),
        }
    }
}
