//! dotdrop: save your dotfiles once, deploy them everywhere.
// ponytail: signatures mirror the original python API
#![allow(clippy::too_many_arguments)]

mod cfg_aggregator;
mod cfg_yaml;
mod commands;
mod comparator;
mod error;
mod ftree;
mod importer;
mod installer;
mod linktype;
mod log;
mod model;
mod options;
mod templategen;
mod uninstaller;
mod updater;
mod utils;

use clap::Parser;
use error::Error;
use options::{Cli, Command, DEFAULT_CONFIG, Options};
use std::process::ExitCode;
use utils::VERSION;

fn report(e: &Error) {
    match e {
        Error::Yaml(m) => log::err(format!("yaml error: {m}")),
        Error::Config(m) => log::err(format!("config error: {m}")),
        Error::Undefined(m) => log::err(format!("dependencies error: {m}")),
        Error::Options(m) => log::err(format!("options error: {m}")),
        other => log::err(other),
    }
}

/// execute the selected command; returns its success
fn exec_command(opts: &Options, conf: &mut cfg_aggregator::CfgAggregator) -> bool {
    let res: error::Result<bool> = match &opts.command {
        Command::Profiles(_) => {
            commands::cmd_list_profiles(opts, conf);
            Ok(true)
        }
        Command::Files(_) => {
            commands::cmd_files(opts, conf);
            Ok(true)
        }
        Command::Install(_) => Ok(commands::cmd_install(opts, conf)),
        Command::Compare(_) => {
            let tmp = utils::get_tmpdir();
            let ret = commands::cmd_compare(opts, &tmp);
            // clean tmp directory, ignore any error
            utils::removepath(&tmp);
            Ok(ret)
        }
        Command::Import(_) => commands::cmd_import(opts, conf),
        Command::Update(_) => Ok(commands::cmd_update(opts, conf)),
        Command::Detail(_) => {
            commands::cmd_detail(opts, conf);
            Ok(true)
        }
        Command::Remove(_) => commands::cmd_remove(opts, conf).map(|_| true),
        Command::Uninstall(_) => {
            commands::cmd_uninstall(opts, conf);
            Ok(true)
        }
        Command::Gencfg => Ok(true),
    };
    match res {
        Ok(r) => r,
        Err(e) => {
            log::err(e);
            false
        }
    }
}

/// options may be given before the command (like with docopt): move the
/// command to the front
fn normalize_args(args: Vec<String>) -> Vec<String> {
    const COMMANDS: [&str; 10] = [
        "install",
        "import",
        "compare",
        "update",
        "remove",
        "uninstall",
        "files",
        "detail",
        "profiles",
        "gencfg",
    ];
    const SHORT_WITH_VALUE: &str = "cpwiCKls";
    const LONG_WITH_VALUE: [&str; 10] = [
        "--cfg",
        "--profile",
        "--workers",
        "--ignore",
        "--file",
        "--dkey",
        "--link",
        "--as",
        "--transr",
        "--transw",
    ];
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a == "--" || !a.starts_with('-') {
            if COMMANDS.contains(&a.as_str()) {
                let mut out = args.clone();
                let cmd = out.remove(i);
                out.insert(1, cmd);
                return out;
            }
            return args;
        }
        if let Some(long) = a.strip_prefix("--") {
            if !long.contains('=') && LONG_WITH_VALUE.contains(&a.as_str()) {
                i += 1;
            }
        } else {
            let chars: Vec<char> = a.chars().skip(1).collect();
            if let Some(pos) = chars.iter().position(|c| SHORT_WITH_VALUE.contains(*c)) {
                if pos == chars.len() - 1 {
                    i += 1;
                }
            }
        }
        i += 1;
    }
    args
}

fn run() -> bool {
    let cli = match Cli::try_parse_from(normalize_args(std::env::args().collect())) {
        Ok(c) => c,
        Err(e) => {
            let ok = !e.use_stderr();
            let _ = e.print();
            return ok;
        }
    };
    if cli.version {
        println!("{VERSION}");
        return true;
    }
    let Some(command) = cli.command else {
        eprintln!("{}", options::banner());
        eprintln!("\nUsage: dotdrop <command> [options], see --help");
        return false;
    };
    if matches!(command, Command::Gencfg) {
        println!("{DEFAULT_CONFIG}");
        return true;
    }
    let (opts, mut conf) = match Options::new(command) {
        Ok(r) => r,
        Err(e) => {
            report(&e);
            return false;
        }
    };
    if opts.settings.check_version {
        utils::check_version();
    }
    let ret = exec_command(&opts, &mut conf);
    dbg_log!("done executing command");
    if ret {
        match conf.save() {
            Ok(true) => log::log("config file updated"),
            Ok(false) => {}
            Err(e) => {
                report(&e);
                return false;
            }
        }
    }
    dbg_log!("return {ret}");
    ret
}

fn main() -> ExitCode {
    if run() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
