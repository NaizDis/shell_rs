#![allow(unused_imports, clippy::enum_variant_names)]
use std::arch::x86_64::CpuidResult;
use std::array::from_ref;
use std::char::ToUppercase;
use std::env::{self, split_paths, var};
use std::fs::Metadata;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path;
use std::process::Output;

pub mod cmd;
use anyhow::Chain;
use cmd::BUILT_IN_COMMANDS;

use crate::cmd::{ChainOp, Command, CompleteAction, DeclareAction, Job, JobStatus};

fn main() {
    Command::load_hist_from_env();
    loop {
        //update process table and push notifaction
        Command::print_job_noti();

        //command
        let input = Command::read_input_with_completion();
        Command::add_history(&input);
        let command = Command::from_input(&input);

        match command {
            Command::ExitCommand => {
                Command::save_to_histfile();
                break;
            }
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
            //history command
            Command::HistoryCommand {
                stdout_file,
                count,
                read_file,
                write_file,
                append_file,
            } => {
                if let Some(path) = read_file {
                    if let Ok(contents) = std::fs::read_to_string(&path) {
                        for line in contents.lines() {
                            Command::add_history(line);
                        }
                    }
                    {
                        let history = Command::history_list().lock().unwrap();
                        *Command::history_written().lock().unwrap() = history.len();
                    }
                } else if let Some(path) = write_file {
                    let content = {
                        let history = Command::history_list().lock().unwrap();
                        let text = history.join("\n") + "\n";
                        *Command::history_written().lock().unwrap() = history.len();
                        text
                    };
                    let _ = std::fs::write(&path, content);
                } else if let Some(path) = append_file {
                    let to_append = {
                        let history = Command::history_list().lock().unwrap();
                        let mut cursor = Command::history_written().lock().unwrap();
                        let start = (*cursor).min(history.len());
                        let text = history[start..].join("\n");
                        *cursor = history.len();
                        text
                    };
                    if !to_append.is_empty() {
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .unwrap();
                        writeln!(file, "{}", to_append).unwrap();
                    }
                } else {
                    let history = Command::history_list().lock().unwrap();
                    let len = history.len();
                    let start = count.map(|n| len.saturating_sub(n)).unwrap_or(0);
                    let text = history
                        .iter()
                        .enumerate()
                        .skip(start)
                        .map(|(i, cmd)| format!("{:>5}    {}", i + 1, cmd))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Some(ref file) = stdout_file {
                        let _ = std::fs::write(file, &text);
                    } else if !text.is_empty() {
                        println!("{}", text);
                    }
                }
            }

            Command::DeclareCommand {
                subcommand,
                stdout_file,
            } => match subcommand {
                DeclareAction::Print { name } => {
                    let output = match Command::shell_vars().lock().unwrap().get(&name) {
                        Some(value) => format!("declare -- {}=\"{}\"", name, value),
                        None => format!("declare: {}: not found", name),
                    };
                    if let Some(ref file) = stdout_file {
                        let _ = std::fs::write(file, &output);
                    } else {
                        println!("{}", output);
                    }
                }
                DeclareAction::Set { name, value } => {
                    Command::shell_vars().lock().unwrap().insert(name, value);
                }
                DeclareAction::Error(msg) => eprintln!("{}", msg),
                DeclareAction::Empty => {}
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
                commands,
                background: false,
            } => {
                let mut prev_output: Option<Vec<u8>> = None;
                for (i, cmd) in commands.iter().enumerate() {
                    let is_last = i == commands.len() - 1;
                    match cmd {
                        Command::ExecCommand { exec_name } => {
                            use std::io::{Read, Write};
                            let mut child = match std::process::Command::new(
                                exec_name.path.as_deref().unwrap_or(&exec_name.name),
                            )
                            .args(&exec_name.args)
                            .stdin(match prev_output {
                                Some(_) => std::process::Stdio::piped(),
                                None => std::process::Stdio::inherit(),
                            })
                            .stdout(if is_last {
                                std::process::Stdio::inherit()
                            } else {
                                std::process::Stdio::piped()
                            })
                            .stderr(std::process::Stdio::inherit())
                            .spawn()
                            {
                                Ok(c) => c,
                                Err(e) => {
                                    eprintln!("{}", e);
                                    continue;
                                }
                            };
                            if let Some(data) = prev_output.take() {
                                if let Some(mut stdin) = child.stdin.take() {
                                    let _ = stdin.write_all(&data);
                                }
                            }
                            if is_last {
                                let _ = child.wait();
                            } else {
                                let mut buf = Vec::new();
                                if let Some(mut stdout) = child.stdout.take() {
                                    let _ = stdout.read_to_end(&mut buf);
                                }
                                let _ = child.wait();
                                prev_output = Some(buf);
                            }
                        }
                        Command::EchoCommand { display_string } => {
                            let output = format!("{}\n", display_string.name).into_bytes();
                            if is_last {
                                print!("{}", String::from_utf8_lossy(&output));
                                use std::io::Write;
                                let _ = std::io::stdout().flush();
                            } else {
                                prev_output = Some(output);
                            }
                        }
                        Command::TypeCommand { command_name } => {
                            let text = if BUILT_IN_COMMANDS.contains(&command_name.name.as_str()) {
                                format!("{} is a shell builtin", command_name.name)
                            } else if let Some(path) = &command_name.path {
                                format!("{} is {}", command_name.name, path)
                            } else {
                                format!("{} not found", command_name.name)
                            };
                            let output = format!("{}\n", text).into_bytes();
                            if is_last {
                                print!("{}", String::from_utf8_lossy(&output));
                                let _ = std::io::stdout().flush();
                            } else {
                                prev_output = Some(output);
                            }
                        }
                        Command::PwdCommand { .. } => {
                            let output = format!("{}\n", Command::pwd_direc()).into_bytes();
                            if is_last {
                                print!("{}", String::from_utf8_lossy(&output));
                                let _ = std::io::stdout().flush();
                            } else {
                                prev_output = Some(output);
                            }
                        }
                        _ => {
                            prev_output = None;
                        }
                    }
                }
            }
            // piped command -- bg == true
            Command::PipeCommand {
                commands,
                background: true,
            } => {
                let mut prev_output: Option<Vec<u8>> = None;
                let mut last_child: Option<std::process::Child> = None;
                for (i, cmd) in commands.iter().enumerate() {
                    let is_last = i == commands.len() - 1;
                    match cmd {
                        Command::ExecCommand { exec_name } => {
                            use std::io::{Read, Write};
                            let mut child = match std::process::Command::new(
                                exec_name.path.as_deref().unwrap_or(&exec_name.name),
                            )
                            .args(&exec_name.args)
                            .stdin(match prev_output {
                                Some(_) => std::process::Stdio::piped(),
                                None => std::process::Stdio::null(),
                            })
                            .stdout(if is_last {
                                std::process::Stdio::inherit()
                            } else {
                                std::process::Stdio::piped()
                            })
                            .stderr(std::process::Stdio::inherit())
                            .spawn()
                            {
                                Ok(c) => c,
                                Err(e) => {
                                    eprintln!("{}", e);
                                    continue;
                                }
                            };
                            if let Some(data) = prev_output.take() {
                                if let Some(mut stdin) = child.stdin.take() {
                                    let _ = stdin.write_all(&data);
                                }
                            }
                            if is_last {
                                last_child = Some(child);
                            } else {
                                let mut buf = Vec::new();
                                if let Some(mut stdout) = child.stdout.take() {
                                    let _ = stdout.read_to_end(&mut buf);
                                }
                                let _ = child.wait();
                                prev_output = Some(buf);
                            }
                        }
                        Command::EchoCommand { display_string } => {
                            let output = format!("{}\n", display_string.name).into_bytes();
                            if is_last {
                                print!("{}", String::from_utf8_lossy(&output));
                                let _ = std::io::stdout().flush();
                            } else {
                                prev_output = Some(output);
                            }
                        }
                        Command::TypeCommand { command_name } => {
                            let text = if BUILT_IN_COMMANDS.contains(&command_name.name.as_str()) {
                                format!("{} is a shell builtin", command_name.name)
                            } else if let Some(path) = &command_name.path {
                                format!("{} is {}", command_name.name, path)
                            } else {
                                format!("{} not found", command_name.name)
                            };
                            let output = format!("{}\n", text).into_bytes();
                            if is_last {
                                print!("{}", String::from_utf8_lossy(&output));
                                let _ = std::io::stdout().flush();
                            } else {
                                prev_output = Some(output);
                            }
                        }
                        Command::PwdCommand { .. } => {
                            let output = format!("{}\n", Command::pwd_direc()).into_bytes();
                            if is_last {
                                print!("{}", String::from_utf8_lossy(&output));
                                let _ = std::io::stdout().flush();
                            } else {
                                prev_output = Some(output);
                            }
                        }
                        _ => {
                            prev_output = None;
                        }
                    }
                }

                let pid = last_child.as_ref().map(|c| c.id()).unwrap_or(0);
                let job_number = {
                    let mut jobs = Command::jobs_list().lock().unwrap();
                    let n = jobs.iter().map(|j| j.job_number).max().unwrap_or(0) + 1;
                    let desc: Vec<&str> = commands
                        .iter()
                        .map(|c| {
                            if let Command::ExecCommand { exec_name } = c {
                                exec_name.name.as_str()
                            } else {
                                "builtin"
                            }
                        })
                        .collect();
                    jobs.push(Job {
                        job_number: n,
                        pid,
                        command: desc.join(" | "),
                        status: JobStatus::Running,
                        notified: false,
                    });
                    n
                };
                println!("[{}] {}", job_number, pid);
                if let Some(mut child) = last_child {
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
