#![allow(unused_imports, clippy::enum_variant_names)]
use std::array::from_ref;
use std::char::ToUppercase;
use std::env::{self, split_paths, var};
use std::fs::Metadata;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

pub mod cmd;
use anyhow::Chain;
use cmd::BUILT_IN_COMMANDS;

use crate::cmd::{ChainOp, Command, CompleteAction, Job, JobStatus};

fn main() {
    loop {
        //update process table and push notifaction
        Command::print_job_noti();

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
                stderr_file: _,
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
            //background == true
            Command::CommandChain {
                segments,
                background: true,
            } => {
                let (tx, rx) = std::sync::mpsc::channel();

                let job_number = {
                    let mut jobs = Command::jobs_list().lock().unwrap();
                    let n = jobs.iter().map(|j| j.job_number).max().unwrap_or(0) + 1;
                    jobs.push(Job {
                        job_number: n,
                        pid: 0,
                        command: segments
                            .iter()
                            .map(|s| s.tokens.join(" "))
                            .collect::<Vec<_>>()
                            .join(" && "),
                        status: JobStatus::Running,
                        notified: false,
                    });
                    n
                };

                std::thread::spawn(move || {
                    let mut last_status = true;
                    let mut sent_pid = false;
                    for segment in &segments {
                        let input = segment.tokens.join(" ");
                        if let Command::ExecCommand { exec_name } = Command::from_input(&input) {
                            match std::process::Command::new(
                                exec_name.path.as_deref().unwrap_or(&exec_name.name),
                            )
                            .args(&exec_name.args)
                            .stdout(std::process::Stdio::inherit())
                            .stderr(std::process::Stdio::inherit())
                            .stdin(std::process::Stdio::null())
                            .spawn()
                            {
                                Ok(mut child) => {
                                    if !sent_pid {
                                        let _ = tx.send(child.id());
                                        sent_pid = true;
                                    }
                                    last_status =
                                        child.wait().map(|s| s.success()).unwrap_or(false);
                                }
                                Err(e) => {
                                    if !sent_pid {
                                        let _ = tx.send(0);
                                        sent_pid = true;
                                    }
                                    eprintln!("{}", e);
                                    last_status = false;
                                }
                            }
                        } else if !sent_pid {
                            let _ = tx.send(0);
                            sent_pid = true;
                        }

                        match segment.operator {
                            ChainOp::And => {
                                if !last_status {
                                    break;
                                }
                            }
                            ChainOp::Or => {
                                if last_status {
                                    break;
                                }
                            }
                            ChainOp::End => {}
                        }
                    }
                    let mut jobs = Command::jobs_list().lock().unwrap();
                    if let Some(job) = jobs.iter_mut().find(|j| j.job_number == job_number) {
                        job.status = JobStatus::Done;
                    }
                });

                let pid = rx.recv().unwrap_or(0);
                {
                    let mut jobs = Command::jobs_list().lock().unwrap();
                    if let Some(job) = jobs.iter_mut().find(|j| j.job_number == job_number) {
                        job.pid = pid;
                    }
                }
                println!("[{}] {}", job_number, pid);
            }
            //background == false
            Command::CommandChain { segments, .. } => {
                let mut last_status;
                for segment in &segments {
                    let input = segment.tokens.join(" ");
                    let cmd = Command::from_input(&input);
                    match cmd {
                        Command::ExecCommand { exec_name } => {
                            let status = Command::rn_exec(&exec_name);
                            match status {
                                Ok(exit) => last_status = exit.success(),
                                Err(_) => last_status = false,
                            }
                        }
                        Command::EchoCommand { display_string } => {
                            if let Some(ref file) = display_string.stdout_file {
                                let _ = std::fs::write(file, &display_string.name);
                            } else {
                                println!("{}", display_string.name);
                            }
                            last_status = true;
                        }
                        Command::TypeCommand { command_name } => {
                            let output = if BUILT_IN_COMMANDS.contains(&command_name.name.as_str())
                            {
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
                            last_status = true;
                        }
                        Command::PwdCommand { stdout_file } => {
                            let output = Command::pwd_direc();
                            if let Some(ref file) = stdout_file {
                                let _ = std::fs::write(file, &output);
                            } else {
                                println!("{}", output);
                            }
                            last_status = true;
                        }
                        Command::CdCommand { directory } => {
                            if let Err(e) = Command::change_directory(directory.name) {
                                eprintln!("{}", e);
                                last_status = false;
                            } else {
                                last_status = true;
                            }
                        }
                        Command::ExitCommand => break,
                        Command::CommandNotFound => {
                            last_status = false;
                        }
                        _ => last_status = true,
                    }
                    match segment.operator {
                        ChainOp::And => {
                            if !last_status {
                                break;
                            }
                        }
                        ChainOp::Or => {
                            if last_status {
                                break;
                            }
                        }
                        ChainOp::End => {}
                    }
                }
            }

            //piped command (bg = false)
            Command::PipeCommand {
                left,
                right,
                background: false,
            } => {
                let left_bytes = match left.as_ref() {
                    Command::ExecCommand { exec_name } => {
                        match std::process::Command::new(
                            exec_name.path.as_deref().unwrap_or(&exec_name.name),
                        )
                        .args(&exec_name.args)
                        .stderr(std::process::Stdio::inherit())
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .spawn()
                        {
                            Ok(mut child) => {
                                let mut buf = Vec::new();
                                use std::io::Read;
                                if let Some(mut stdout) = child.stdout.take() {
                                    let _ = stdout.read_to_end(&mut buf);
                                }
                                let _ = child.wait();
                                buf
                            }
                            Err(e) => {
                                eprintln!("{}", e);
                                continue;
                            }
                        }
                    }
                    Command::EchoCommand { display_string } => {
                        format!("{}\n", display_string.name).into_bytes()
                    }
                    Command::TypeCommand { command_name } => {
                        let output = if BUILT_IN_COMMANDS.contains(&command_name.name.as_str()) {
                            format!("{} is a shell builtin", command_name.name)
                        } else if let Some(ref path) = command_name.path {
                            format!("{} is {}", command_name.name, path)
                        } else {
                            format!("{} not found", command_name.name)
                        };
                        format!("{}\n", output).into_bytes()
                    }
                    Command::PwdCommand { .. } => {
                        format!("{}\n", Command::pwd_direc()).into_bytes()
                    }
                    _ => Vec::new(),
                };
                match right.as_ref() {
                    Command::ExecCommand { exec_name } => {
                        use std::io::Write;
                        match std::process::Command::new(
                            exec_name.path.as_deref().unwrap_or(&exec_name.name),
                        )
                        .args(&exec_name.args)
                        .stdin(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::inherit())
                        .stdout(std::process::Stdio::inherit())
                        .spawn()
                        {
                            Ok(mut child) => {
                                if let Some(mut stdin) = child.stdin.take() {
                                    let _ = stdin.write_all(&left_bytes);
                                }
                                let _ = child.wait();
                            }
                            Err(e) => eprintln!("{}", e),
                        }
                    }
                    _ => {
                        Command::execute_builtin(right.as_ref());
                    }
                }
            }
            // piped command -- bg == true
            Command::PipeCommand {
                left,
                right,
                background: true,
            } => {
                let left_bytes = match left.as_ref() {
                    Command::ExecCommand { exec_name } => {
                        match std::process::Command::new(
                            exec_name.path.as_deref().unwrap_or(&exec_name.name),
                        )
                        .args(&exec_name.args)
                        .stderr(std::process::Stdio::inherit())
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .spawn()
                        {
                            Ok(mut child) => {
                                let mut buf = Vec::new();
                                use std::io::Read;
                                if let Some(mut stdout) = child.stdout.take() {
                                    let _ = stdout.read_to_end(&mut buf);
                                }
                                let _ = child.wait();
                                buf
                            }
                            Err(e) => {
                                eprintln!("{}", e);
                                continue;
                            }
                        }
                    }
                    Command::EchoCommand { display_string } => {
                        format!("{}\n", display_string.name).into_bytes()
                    }
                    Command::TypeCommand { command_name } => {
                        let output = if BUILT_IN_COMMANDS.contains(&command_name.name.as_str()) {
                            format!("{} is a shell builtin", command_name.name)
                        } else if let Some(ref path) = command_name.path {
                            format!("{} is {}", command_name.name, path)
                        } else {
                            format!("{} not found", command_name.name)
                        };
                        format!("{}\n", output).into_bytes()
                    }
                    Command::PwdCommand { .. } => {
                        format!("{}\n", Command::pwd_direc()).into_bytes()
                    }
                    _ => Vec::new(),
                };

                let mut right_child: Option<std::process::Child> = None;
                let pid = match right.as_ref() {
                    Command::ExecCommand { exec_name } => {
                        use std::io::Write;
                        match std::process::Command::new(
                            exec_name.path.as_deref().unwrap_or(&exec_name.name),
                        )
                        .args(&exec_name.args)
                        .stdin(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::inherit())
                        .stdout(std::process::Stdio::inherit())
                        .spawn()
                        {
                            Ok(mut child) => {
                                let pid = child.id();
                                child
                                    .stdin
                                    .as_mut()
                                    .map(|stdin| stdin.write_all(&left_bytes));
                                right_child = Some(child);
                                pid
                            }
                            Err(e) => {
                                eprintln!("{}", e);
                                continue;
                            }
                        }
                    }
                    _ => {
                        Command::execute_builtin(right.as_ref());
                        0
                    }
                };

                let job_number = {
                    let mut jobs = Command::jobs_list().lock().unwrap();
                    let n = jobs.iter().map(|j| j.job_number).max().unwrap_or(0) + 1;
                    let desc_left = if let Command::ExecCommand { exec_name } = left.as_ref() {
                        &exec_name.name
                    } else {
                        "builtin"
                    };
                    let desc_right = if let Command::ExecCommand { exec_name } = right.as_ref() {
                        &exec_name.name
                    } else {
                        "builtin"
                    };
                    jobs.push(Job {
                        job_number: n,
                        pid,
                        command: format!("{} | {}", desc_left, desc_right),
                        status: JobStatus::Running,
                        notified: false,
                    });
                    n
                };
                println!("[{}] {}", job_number, pid);
                if let Some(mut child) = right_child {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                        let mut jobs = Command::jobs_list().lock().unwrap();
                        if let Some(job) = jobs.iter_mut().find(|j| j.job_number == job_number) {
                            job.status = JobStatus::Done;
                        }
                    });
                }
            }

            Command::JobsCommand { stdout_file } => {
                let mut jobs = Command::jobs_list().lock().unwrap();
                let max_job = jobs.iter().map(|j| j.job_number).max();
                let second_max = max_job.and_then(|m| {
                    jobs.iter()
                        .filter(|j| j.job_number != m)
                        .map(|j| j.job_number)
                        .max()
                });
                let text = jobs
                    .iter()
                    .map(|j| {
                        let marker = if Some(j.job_number) == max_job {
                            "+"
                        } else if Some(j.job_number) == second_max {
                            "-"
                        } else {
                            " "
                        };
                        match j.status {
                            JobStatus::Running => format!(
                                "[{}]{}  Running                 {}&",
                                j.job_number, marker, j.command
                            ),
                            JobStatus::Done => format!(
                                "[{}]{}  Done                    {}",
                                j.job_number, marker, j.command
                            ),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                jobs.retain(|j| !matches!(j.status, JobStatus::Done));
                if let Some(ref file) = stdout_file {
                    let _ = std::fs::write(file, &text);
                } else if !text.is_empty() {
                    println!("{}", text);
                }
            }
            Command::ExecCommand { exec_name } => {
                if exec_name.background {
                    let path = match exec_name.path.as_deref() {
                        Some(p) => p.to_string(),
                        None => {
                            eprintln!("No Executable found for '{}'", exec_name.name);
                            continue;
                        }
                    };
                    match std::process::Command::new(&path)
                        .args(&exec_name.args)
                        .stdin(std::process::Stdio::null())
                        .stderr(std::process::Stdio::inherit())
                        .stdout(std::process::Stdio::inherit())
                        .spawn()
                    {
                        Ok(mut child) => {
                            let pid = child.id();
                            let job_number = {
                                let mut jobs = Command::jobs_list().lock().unwrap();
                                let n = jobs.iter().map(|j| j.job_number).max().unwrap_or(0) + 1;
                                jobs.push(Job {
                                    job_number: n,
                                    pid,
                                    command: exec_name.name.clone(),
                                    status: JobStatus::Running,
                                    notified: false,
                                });
                                n
                            };
                            println!("[{}] {}", job_number, pid);

                            //Monitor Thread -- read child + update status
                            std::thread::spawn(move || {
                                let _ = child.wait();
                                let mut jobs = Command::jobs_list().lock().unwrap();
                                if let Some(job) = jobs.iter_mut().find(|j| j.pid == pid) {
                                    job.status = JobStatus::Done;
                                }
                            });
                        }
                        Err(e) => eprintln!("Failed to spawn: {}", e),
                    }
                } else {
                    if let Err(e) = Command::rn_exec(&exec_name) {
                        eprintln!("{}", e)
                    }
                }
            }
            Command::Noop => {}
            Command::CommandNotFound => println!("{} command not found!!", input.trim()),
        }
    }
}
