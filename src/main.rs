#![allow(unused_imports, clippy::enum_variant_names)]
use std::env::{self, split_paths, var};
use std::fs::Metadata;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;

pub mod cmd;
use cmd::{BUILT_IN_COMMANDS, Command};

fn main() {
    loop {
        print!("{} $ ", Command::pwd_direc());
        io::stdout().flush().unwrap();

        //command
        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        let command = Command::from_input(&input);

        match command {
            Command::ExitCommand => break,
            Command::PwdCommand => println!("{}", Command::pwd_direc()),
            Command::CdCommand { directory } => {
                if let Err(e) = Command::change_directory(directory.name) {
                    eprintln!("{}", e)
                }
            }
            Command::EchoCommand { display_string } => println!("{}", display_string.name),
            Command::TypeCommand { command_name } => {
                let mut flag = false;
                if BUILT_IN_COMMANDS.contains(&command_name.name.as_str()) {
                    println!("{} is a shell builtin", command_name.name);
                    flag = true;
                } else if let Some(path) = command_name.path {
                    flag = true;
                    println!("{} is {}", command_name.name, path);
                }
                if !flag {
                    println!("{} not found", command_name.name);
                }
            }
            Command::ExecCommand { exec_name } => {
                if let Err(e) = Command::rn_exec(&exec_name) {
                    eprintln!("{}", e);
                }
            }
            Command::CommandNotFound => println!("{} command not found!!", input.trim()),
        }
    }
}
