#![allow(clippy::enum_variant_names, non_camel_case_types)]
use anyhow::{Chain, Context, anyhow};
use std::cell::Ref;
use std::collections::HashMap;
use std::env::{self, split_paths, var};
use std::fs::{Metadata, write};
use std::hash::Hash;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{self, Path};
use std::process::{Output, Stdio};
use std::slice::SliceIndex;
use std::str::CharIndices;
use std::sync::OnceLock;
use std::sync::mpsc::Receiver;
use termion::cursor::Right;
use termion::{event::Key, input::TermRead, raw::IntoRawMode};

pub const BUILT_IN_COMMANDS: [&str; 8] = [
    "echo", "type", "exit", "pwd", "complete", "jobs", "history", "declare",
];

pub enum Command {
    ExitCommand,
    PwdCommand {
        stdout_file: Option<String>,
    },
    CdCommand {
        directory: exeCmd,
    },
    EchoCommand {
        display_string: exeCmd,
    },
    TypeCommand {
        command_name: exeCmd,
    },
    ExecCommand {
        exec_name: exeCmd,
    },
    CompleteCommand {
        subcommnad: CompleteAction,
        stdout_file: Option<String>,
        stderr_file: Option<String>,
    },
    JobsCommand {
        stdout_file: Option<String>,
    },
    HistoryCommand {
        stdout_file: Option<String>,
        count: Option<usize>,
        read_file: Option<String>,
        write_file: Option<String>,
        append_file: Option<String>,
    },
    DeclareCommand {
        subcommand: DeclareAction,
        stdout_file: Option<String>,
    },
    CommandChain {
        segments: Vec<ChainSegment>,
        background: bool,
    },
    PipeCommand {
        commands: Vec<Command>,
        background: bool,
    },
    Noop,
    CommandNotFound,
}

pub enum ChainOp {
    And,
    Or,
    End,
}
pub struct ChainSegment {
    pub tokens: Vec<String>,
    pub operator: ChainOp,
}

enum TabCompletion {
    Single(String),
    Multiple(Vec<String>),
    None,
}

#[derive(Clone)]
pub enum JobStatus {
    Running,
    Done,
}

#[derive(Clone)]
pub struct Job {
    pub job_number: usize,
    pub pid: u32,
    pub command: String,
    pub status: JobStatus,
    pub notified: bool,
}

pub enum CompleteAction {
    Register { script: String, command: String },
    Print { command: String },
    Remove { command: String },
    Error(String),
    Empty,
}

pub enum DeclareAction {
    Print { name: String },
    Set { name: String, value: String },
    Error(String),
    Empty,
}

impl Command {
    //return command type from input
    pub fn from_input(input: &str) -> Self {
        let input = input.trim();
        if input.is_empty() {
            return Command::Noop;
        }
        let tokens = Self::tokenize(input);
        let (mut cmd_tokens, stdop_file, stdout_append, stderr_file, stderr_append) = {
            let mut std_op = None;
            let mut std_append = false;
            let mut stderr = None;
            let mut err_append = false;
            let mut cmd_only = Vec::new();
            let mut i = 0;
            while i < tokens.len() {
                if tokens[i] == ">>" && i + 1 < tokens.len() {
                    std_op = Some(tokens[i + 1].clone());
                    std_append = true;
                    i += 2;
                    continue;
                }
                if tokens[i] == "2>>" && i + 1 < tokens.len() {
                    stderr = Some(tokens[i + 1].clone());
                    err_append = true;
                    i += 2;
                    continue;
                }
                if (tokens[i] == ">" || tokens[i] == "1>") && i + 1 < tokens.len() {
                    std_op = Some(tokens[i + 1].clone());
                    i += 2;
                    continue;
                }
                if tokens[i] == "2>" && i + 1 < tokens.len() {
                    stderr = Some(tokens[i + 1].clone());
                    i += 2;
                    continue;
                }
                cmd_only.push(tokens[i].clone());
                i += 1;
            }
            (cmd_only, std_op, std_append, stderr, err_append)
        };

        //expand $VAR tokens before parsing
        cmd_tokens = cmd_tokens
            .into_iter()
            .map(|t| Self::expand_param(&t))
            .collect();

        //check for background opeartr
        let background = cmd_tokens.last().map(|s| s == "&").unwrap_or(false);
        if background {
            cmd_tokens.pop();
        }

        //pipe check
        let pipe_count = cmd_tokens.iter().filter(|t| *t == "|").count();
        if pipe_count > 0 {
            let mut pipe_cmds = Vec::new();
            let mut current = Vec::new();
            for token in &cmd_tokens {
                if token == "|" {
                    if current.is_empty() {
                        return Command::CommandNotFound;
                    }
                    pipe_cmds.push(current);
                    current = Vec::new();
                } else if token == "&&" || token == "||" {
                    return Command::CommandNotFound;
                } else {
                    current.push(token.clone());
                }
            }
            if current.is_empty() {
                return Command::CommandNotFound;
            }
            pipe_cmds.push(current);

            let mut commands = Vec::new();
            for tokens in pipe_cmds {
                let input = tokens.join(" ");
                let cmd = Command::from_input(&input);
                if matches!(
                    cmd,
                    Command::Noop | Command::CommandNotFound | Command::ExitCommand
                ) {
                    return Command::CommandNotFound;
                }
                commands.push(cmd);
            }
            return Command::PipeCommand {
                commands,
                background,
            };
        }

        //check for && and || in cmd
        let has_chain = cmd_tokens.iter().any(|t| t == "&&" || t == "||");
        if has_chain {
            let mut segments = Vec::new();
            let mut current = Vec::new();
            for token in &cmd_tokens {
                if token == "&&" {
                    segments.push(ChainSegment {
                        tokens: current,
                        operator: ChainOp::And,
                    });
                    current = Vec::new();
                } else if token == "||" {
                    segments.push(ChainSegment {
                        tokens: current,
                        operator: ChainOp::Or,
                    });
                    current = Vec::new();
                } else {
                    current.push(token.clone());
                }
            }
            if !current.is_empty() {
                segments.push(ChainSegment {
                    tokens: current,
                    operator: ChainOp::End,
                });
            }
            return Command::CommandChain {
                segments,
                background,
            };
        }

        if cmd_tokens.is_empty() {
            return Command::Noop;
        }

        let cmd = cmd_tokens[0].as_str();
        let args = cmd_tokens[1..].join(" ");

        match cmd {
            "exit" => Command::ExitCommand,
            "echo" => Command::EchoCommand {
                display_string: exeCmd {
                    name: args,
                    path: None,
                    args: vec![],
                    stdout_file: stdop_file,
                    stdout_append,
                    stderr_file: None,
                    stderr_append: false,
                    background,
                },
            },
            "type" => {
                let parse = Command::parse_input(&args)
                    .unwrap_or_else(|_| exeCmd::new(&args, None, vec![]));
                Command::TypeCommand {
                    command_name: parse,
                }
            }
            "pwd" => Command::PwdCommand {
                stdout_file: stdop_file,
            },
            "cd" => {
                let parse = Command::parse_input(&args)
                    .unwrap_or_else(|_| exeCmd::new(&args, None, vec![]));
                Command::CdCommand { directory: parse }
            }
            "complete" => {
                let tokens = Self::tokenize(&args);
                let action = match tokens.as_slice() {
                    [flag, script, cmd] if flag == "-C" => CompleteAction::Register {
                        script: script.clone(),
                        command: cmd.clone(),
                    },
                    [flag, cmd] if flag == "-p" => CompleteAction::Print {
                        command: cmd.clone(),
                    },
                    [flag, cmd] if flag == "-r" => CompleteAction::Remove {
                        command: cmd.clone(),
                    },
                    [] => CompleteAction::Empty,
                    _ => CompleteAction::Error("complete: unrecognised flag".to_string()),
                };
                Command::CompleteCommand {
                    subcommnad: action,
                    stdout_file: stdop_file,
                    stderr_file,
                }
            }
            "jobs" => Command::JobsCommand {
                stdout_file: stdop_file,
            },
            "history" => {
                let mut read_file = None;
                let mut write_file = None;
                let mut append_file = None;
                let toks = Self::tokenize(&args);
                if let [flag, path] = toks.as_slice() {
                    match flag.as_str() {
                        "-r" => read_file = Some(path.clone()),
                        "-w" => write_file = Some(path.clone()),
                        "-a" => append_file = Some(path.clone()),
                        _ => {}
                    }
                }
                let count = if read_file.is_none()
                    && write_file.is_none()
                    && append_file.is_none()
                    && !args.trim().is_empty()
                {
                    args.trim().parse::<usize>().ok()
                } else {
                    None
                };
                Command::HistoryCommand {
                    stdout_file: stdop_file,
                    count,
                    read_file,
                    write_file,
                    append_file,
                }
            }
            "declare" => {
                let toks = Self::tokenize(&args);
                let subcommand = match toks.as_slice() {
                    [flag, name] if flag == "-p" => DeclareAction::Print { name: name.clone() },
                    [assignment] => {
                        if assignment.starts_with('-') {
                            DeclareAction::Error("declare: unrecognised flag".to_string())
                        } else {
                            let (name, value) = assignment
                                .split_once('=')
                                .map_or((assignment.clone(), String::new()), |(n, v)| {
                                    (n.to_string(), v.to_string())
                                });
                            if Self::valid_identifier(&name) {
                                DeclareAction::Set { name, value }
                            } else {
                                DeclareAction::Error(format!(
                                    "declare : `{}': not valid indentifier",
                                    assignment
                                ))
                            }
                        }
                    }
                    [] => DeclareAction::Empty,
                    _ => DeclareAction::Error("declare: unrecognised flag".to_string()),
                };
                Command::DeclareCommand {
                    subcommand,
                    stdout_file: stdop_file,
                }
            }
            _ => {
                let clean = cmd_tokens.join(" ");
                if let Ok(mut data) = Command::parse_input(&clean) {
                    data.stdout_file = stdop_file;
                    data.stdout_append = stdout_append;
                    data.stderr_file = stderr_file;
                    data.stderr_append = stderr_append;
                    data.background = background;
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
        if parts.is_empty() {
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
            stdout_file: None,
            stdout_append: false,
            stderr_file: None,
            stderr_append: false,
            background: false,
        })
    }

    //expand sime $NAME / ${NAME} referece with shell variables value
    fn expand_param(tok: &str) -> String {
        let vars = Self::shell_vars().lock().unwrap();
        let mut out = String::new();
        let mut rest = tok;
        while let Some(idx) = rest.find('$') {
            out.push_str(&rest[..idx]);
            let after = &rest[idx + 1..];
            if let Some(inner) = after.strip_prefix('{') {
                match inner.find('}') {
                    Some(close) => {
                        let name = &inner[..close];
                        if let Some(v) = vars.get(name) {
                            out.push_str(v);
                        }
                        rest = &inner[close + 1..];
                    }
                    None => {
                        out.push('$');
                        rest = after
                    }
                }
            } else {
                let end = after
                    .char_indices()
                    .find_map(|(i, c)| (!(c.is_ascii_alphanumeric() || c == '_')).then_some(i))
                    .unwrap_or(after.len());
                let name = &after[..end];
                if name.is_empty() {
                    out.push('$');
                    rest = after;
                } else {
                    if let Some(v) = vars.get(name) {
                        out.push_str(v);
                    }
                    rest = &after[end..];
                }
            }
        }
        out.push_str(rest);
        out
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

    //shell vraible indentifier valid - start with letter or _ ,rest anything
    fn valid_identifier(name: &str) -> bool {
        let mut chars = name.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    //current working directory
    pub fn pwd_direc() -> String {
        env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "?/?".to_string())
    }

    //completions storeage
    pub fn completions() -> &'static std::sync::Mutex<HashMap<String, String>> {
        static COMPLETIONS: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
        COMPLETIONS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
    }

    //history list
    pub fn history_list() -> &'static std::sync::Mutex<Vec<String>> {
        static HISTORY: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();
        HISTORY.get_or_init(|| std::sync::Mutex::new(Vec::new()))
    }

    //add to history
    pub fn add_history(cmd: &str) {
        if !cmd.trim().is_empty() {
            Self::history_list().lock().unwrap().push(cmd.to_string());
        }
    }

    //load history from file on startup
    pub fn load_hist_from_env() {
        if let Ok(path) = env::var("HISTFILE") {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                for line in contents.lines() {
                    Self::add_history(line);
                }
            }
            let history = Self::history_list().lock().unwrap();
            *Self::history_written().lock().unwrap() = history.len();
        }
    }

    //save to history file on exit
    pub fn save_to_histfile() {
        if let Ok(path) = env::var("HISTFILE") {
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
        }
    }

    //number of history entries already persisted to a file
    pub fn history_written() -> &'static std::sync::Mutex<usize> {
        static WRITTEN: OnceLock<std::sync::Mutex<usize>> = OnceLock::new();
        WRITTEN.get_or_init(|| std::sync::Mutex::new(0))
    }

    //Jobs list
    pub fn jobs_list() -> &'static std::sync::Mutex<Vec<Job>> {
        static JOBS: OnceLock<std::sync::Mutex<Vec<Job>>> = OnceLock::new();
        JOBS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
    }

    //job notifiaction
    pub fn print_job_noti() {
        let mut jobs = Self::jobs_list().lock().unwrap();
        let max_job = jobs.iter().map(|j| j.job_number).max();
        let second_max = max_job.and_then(|m| {
            jobs.iter()
                .filter(|j| j.job_number != m)
                .map(|j| j.job_number)
                .max()
        });
        for job in jobs.iter_mut() {
            if matches!(job.status, JobStatus::Done) && !job.notified {
                let marker = if Some(job.job_number) == max_job {
                    "+"
                } else if Some(job.job_number) == second_max {
                    "-"
                } else {
                    " "
                };
                println!(
                    "[{}]{} Done            {}",
                    job.job_number, marker, job.command
                );
                job.notified = true;
            }
        }
        jobs.retain(|j| !matches!(j.status, JobStatus::Done) || !j.notified);
    }

    //shell varaible storeage
    pub fn shell_vars() -> &'static std::sync::Mutex<HashMap<String, String>> {
        static VARS: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
        VARS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
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
        let mut cmd = std::process::Command::new(path);
        cmd.args(&mdata.args);
        cmd.stdin(std::process::Stdio::inherit());
        if let Some(ref file) = mdata.stdout_file {
            let f = if mdata.stdout_append {
                std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(file)
                    .context("Failed to open file for append")?
            } else {
                std::fs::File::create(file).context("Failed to Create redirect file")?
            };
            cmd.stdout(f);
        } else {
            cmd.stdout(std::process::Stdio::inherit());
        }
        if let Some(ref file) = mdata.stderr_file {
            let f = if mdata.stderr_append {
                std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(file)
                    .context("Failed to open stderr file for append")?
            } else {
                std::fs::File::create(file).context("Failed to Create stderr redirect file")?
            };
            cmd.stderr(f);
        } else {
            cmd.stderr(std::process::Stdio::inherit());
        }
        let mut child = cmd.spawn().context("Failed to spawn a process")?;
        child.wait().context("failed to wait on child")
    }

    //tab_completion

    //LCP
    fn longest_common_prefix(names: &[String]) -> String {
        if names.is_empty() {
            return String::new();
        }
        if names.len() == 1 {
            return names[0].clone();
        }

        let first = &names[0];
        let mut prefix_len = first.len();
        for name in &names[1..] {
            let common = first
                .chars()
                .zip(name.chars())
                .take_while(|(a, b)| a == b)
                .count();
            prefix_len = prefix_len.min(common);
        }
        first[..prefix_len].to_string()
    }

    // Actual Tab completion call
    fn tab_completion(buffer: &str) -> TabCompletion {
        let last_space = buffer.rfind(' ').map(|i| i + 1).unwrap_or(0);
        let prefix = &buffer[last_space..];

        if last_space > 0 {
            let first_space = buffer.find(' ').unwrap();
            let command = &buffer[..first_space];
            let prefix = &buffer[last_space..];

            let prev_word = {
                let start = first_space + 1;
                let end = last_space - 1;
                if start <= end {
                    buffer[start..end].trim().to_string()
                } else {
                    String::new()
                }
            };

            let map = Self::completions().lock().unwrap();
            if let Some(script) = map.get(command) {
                if let Ok(output) = std::process::Command::new(script)
                    .arg(command)
                    .arg(prefix)
                    .arg(&prev_word)
                    .env("COMP_LINE", buffer)
                    .env("COMP_POINT", buffer.len().to_string())
                    .stderr(std::process::Stdio::inherit())
                    .output()
                {
                    let output_text = String::from_utf8_lossy(&output.stdout);
                    let lines: Vec<String> =
                        output_text.lines().map(|l| l.trim().to_string()).collect();

                    match lines.len() {
                        0 => {} //normal completion
                        1 => return TabCompletion::Single(format!("{} ", lines[0])),
                        _ => {
                            let mut sortes = lines;
                            sortes.sort();
                            let lcp = Self::longest_common_prefix(&sortes);
                            if lcp.len() > prefix.len() {
                                return TabCompletion::Single(lcp); //LCP found 
                            } else {
                                return TabCompletion::Multiple(sortes);
                            }
                        }
                    }
                }
            }
        }

        if prefix.is_empty() {
            return TabCompletion::Multiple(Vec::new());
        }

        // tab after command/first word
        if last_space > 0 || prefix.contains('/') || prefix.starts_with('.') {
            return Self::complete_path(prefix);
        }

        // tab at first word
        let mut candidates = Vec::new();
        //builtin prefercne
        for name in &["echo", "exit"] {
            if name.starts_with(prefix) {
                candidates.push(name.to_string());
            }
        }

        //exter path execs
        for exe in Self::get_exec_by_prefix(prefix) {
            if !candidates.contains(&exe) {
                candidates.push(exe);
            }
        }

        match candidates.len() {
            0 => TabCompletion::None,
            1 => TabCompletion::Single(format!("{} ", candidates[0])),
            _ => {
                let lcp = Self::longest_common_prefix(&candidates);
                if lcp.len() > prefix.len() {
                    TabCompletion::Single(lcp)
                } else {
                    TabCompletion::Multiple(candidates)
                }
            }
        }
    }

    // Get executable from path varaible by autocomplete
    fn get_exec_by_prefix(prefix: &str) -> Vec<String> {
        let mut mathces = Vec::new();
        if let Ok(path_env) = var("PATH") {
            for path in split_paths(&path_env) {
                if let Ok(entries) = std::fs::read_dir(&path) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if name.starts_with(prefix) && !name.starts_with('.') {
                            if let Ok(meta) = entry.metadata() {
                                if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                                    mathces.push(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        mathces.sort();
        mathces.dedup();
        mathces
    }

    //Completed Path
    fn complete_path(prefix: &str) -> TabCompletion {
        let (dir, file_prefix) = match prefix.rfind('/') {
            Some(i) => ((prefix[..=i]).to_string(), &prefix[i + 1..]),
            None => (String::new(), prefix),
        };
        let search_dir = if dir.is_empty() { "." } else { &dir };

        let mut matches = Vec::new();
        if let Ok(entries) = std::fs::read_dir(search_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(file_prefix) && !name.starts_with('.') {
                    let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
                    let suffix = if is_dir { "/" } else { " " };
                    matches.push(name + suffix);
                }
            }
        }
        matches.sort();

        match matches.len() {
            0 => TabCompletion::None,
            1 => {
                let is_dir = std::fs::metadata(search_dir.to_string() + &matches[0])
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                let suffix = if is_dir { "/" } else { " " };
                TabCompletion::Single(dir.clone() + &matches[0] + suffix)
            }
            _ => {
                let lcp = Self::longest_common_prefix(&matches);
                if lcp.len() > file_prefix.len() {
                    TabCompletion::Single(dir + &lcp)
                } else {
                    //Build Dispaly with suffix
                    let display = matches
                        .iter()
                        .map(|n| {
                            let full = dir.clone() + n;
                            let is_dir = std::fs::metadata(search_dir.to_string() + &matches[0])
                                .map(|m| m.is_dir())
                                .unwrap_or(false);
                            let suffix = if is_dir { "/" } else { " " };
                            full + suffix
                        })
                        .collect();
                    TabCompletion::Multiple(display)
                }
            }
        }
    }

    pub fn read_input_with_completion() -> String {
        let mut stdout = io::stdout().into_raw_mode().unwrap();
        let mut buffer = String::new();

        write!(stdout, "{} $ ", Self::pwd_direc()).unwrap();
        stdout.flush().unwrap();

        let mut prev_tab_buffer: Option<String> = None;
        let mut saved_buffer: Option<String> = None;
        let mut hist_pos: Option<usize> = None;

        for key in io::stdin().keys() {
            match key.unwrap() {
                Key::Char('\n') | Key::Char('\r') => {
                    write!(stdout, "\r\n").unwrap();
                    break;
                }
                Key::Char('\t') => match Self::tab_completion(&buffer) {
                    TabCompletion::Single(s) => {
                        let last_space = buffer.rfind(' ').map(|i| i + 1).unwrap_or(0);
                        let completed = buffer[..last_space].to_string() + &s;
                        write!(stdout, "\r\x1b[K{} $ {}", Command::pwd_direc(), completed).unwrap();
                        buffer = completed;
                        prev_tab_buffer = None;
                    }
                    TabCompletion::Multiple(list) if list.is_empty() => {
                        prev_tab_buffer = None;
                        write!(stdout, "\x07").unwrap();
                    }
                    TabCompletion::Multiple(list) => {
                        // second tab same buffer === dispaly
                        if prev_tab_buffer.as_deref() == Some(buffer.as_str()) {
                            write!(stdout, "\r\n{}\r\n", list.join("        ")).unwrap();
                            write!(stdout, "{} $ {}", Command::pwd_direc(), buffer).unwrap();
                            prev_tab_buffer = None;
                        } else {
                            // First tab === ring bell ,save for later
                            write!(stdout, "\x07").unwrap();
                            prev_tab_buffer = Some(buffer.clone())
                        }
                    }
                    TabCompletion::None => {
                        write!(stdout, "\x07").unwrap();
                        prev_tab_buffer = None;
                    }
                },
                Key::Backspace => {
                    if !buffer.is_empty() {
                        buffer.pop();
                        write!(stdout, "\r\x1b[K{} $ {}", Self::pwd_direc(), buffer).unwrap();
                    }
                }
                Key::Char(c) => {
                    buffer.push(c);
                    write!(stdout, "{}", c).unwrap();
                }
                Key::Up => {
                    let history = Self::history_list().lock().unwrap();
                    if history.is_empty() {
                        continue;
                    }
                    if hist_pos.is_none() {
                        saved_buffer = Some(buffer.clone());
                    }
                    let pos = match hist_pos {
                        Some(i) => i.saturating_sub(1),
                        None => history.len() - 1,
                    };
                    hist_pos = Some(pos);
                    buffer = history[pos].clone();
                    prev_tab_buffer = None;
                    write!(stdout, "\r\x1b[K{} $ {}", Self::pwd_direc(), buffer).unwrap();
                }
                Key::Down => {
                    if let Some(i) = hist_pos {
                        let history = Self::history_list().lock().unwrap();
                        if i + 1 < history.len() {
                            hist_pos = Some(i + 1);
                            buffer = history[i + 1].clone();
                        } else {
                            hist_pos = None;
                            buffer = saved_buffer.take().unwrap_or_default();
                        }
                        prev_tab_buffer = None;
                        write!(stdout, "\r\x1b[K{} $ {}", Self::pwd_direc(), buffer).unwrap();
                    }
                }
                Key::Ctrl('c') => {
                    write!(stdout, "^C\r\n").unwrap();
                    buffer = "exit".to_string();
                    break;
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
        // Raw mode drops when stdout goes out of scope; terminal restores automatically
        buffer
    }
}

pub struct exeCmd {
    pub name: String,
    pub path: Option<String>,
    pub args: Vec<String>,
    pub stdout_file: Option<String>,
    pub stdout_append: bool,
    pub stderr_file: Option<String>,
    pub stderr_append: bool,
    pub background: bool,
}

impl exeCmd {
    pub fn new(name: &str, path: Option<String>, args: Vec<String>) -> Self {
        Self {
            name: name.to_owned(),
            path,
            args,
            stdout_file: None,
            stdout_append: false,
            stderr_file: None,
            stderr_append: false,
            background: false,
        }
    }
}
