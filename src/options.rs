//! Command line parsing and the effective options.

use crate::cfg_aggregator::CfgAggregator;
use crate::dbg_log;
use crate::error::{Error, Result};
use crate::linktype::LinkType;
use crate::log;
use crate::model::{Action, Dotfile, Settings};
use crate::templategen::Vars;
use crate::utils::{self, VERSION, uniq_list};
use clap::{Args, Parser, Subcommand};
use std::sync::Arc;

pub const ENV_NOBANNER: &str = "DOTDROP_NOBANNER";
pub const ENV_DEBUG: &str = "DOTDROP_DEBUG";
pub const ENV_NODEBUG: &str = "DOTDROP_FORCE_NODEBUG";
pub const ENV_XDG: &str = "XDG_CONFIG_HOME";
pub const ENV_WORKERS: &str = "DOTDROP_WORKERS";
pub const ENV_PROFILE: &str = "DOTDROP_PROFILE";
pub const ENV_CONFIG: &str = "DOTDROP_CONFIG";
pub const BACKUP_SUFFIX: &str = ".dotdropbak";

const NAME: &str = "dotdrop";
const CONFIGFILEYAML: &str = "config.yaml";
const CONFIGFILETOML: &str = "config.toml";
const HOMECFG: &str = "~/.config/dotdrop";
const ETCXDGCFG: &str = "/etc/xdg/dotdrop";
const ETCCFG: &str = "/etc/dotdrop";

pub const DEFAULT_CONFIG: &str = "config:
  backup: true
  banner: true
  create: true
  dotpath: dotfiles
  keepdot: false
  link_dotfile_default: nolink
  link_on_import: nolink
  longkey: false
dotfiles:
profiles:";

pub fn banner() -> String {
    format!(
        r"     _       _      _
  __| | ___ | |_ __| |_ __ ___  _ __
 / _` |/ _ \| __/ _` | '__/ _ \| '_ |
 \__,_|\___/ \__\__,_|_|  \___/| .__/  v{VERSION}
                               |_|"
    )
}

#[derive(Parser, Debug)]
#[command(
    name = "dotdrop",
    about = "Save your dotfiles once, deploy them everywhere",
    disable_version_flag = true
)]
pub struct Cli {
    /// Show version.
    #[arg(short = 'v', long = "version")]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args, Debug, Clone)]
pub struct Common {
    /// Be verbose.
    #[arg(short = 'V', long)]
    pub verbose: bool,
    /// Do not display the banner.
    #[arg(short = 'b', long = "no-banner")]
    pub no_banner: bool,
    /// Path to the config.
    #[arg(short = 'c', long = "cfg", value_name = "path", env = ENV_CONFIG)]
    pub cfg: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct Prof {
    /// Specify the profile to use [default: hostname].
    #[arg(short = 'p', long = "profile", value_name = "profile", env = ENV_PROFILE)]
    pub profile: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(flatten)]
    pub prof: Prof,
    /// Execute all actions even if no dotfile is installed.
    #[arg(short = 'a', long = "force-actions")]
    pub force_actions: bool,
    /// Dry run.
    #[arg(short = 'd', long)]
    pub dry: bool,
    /// Show a diff before overwriting.
    #[arg(short = 'D', long)]
    pub showdiff: bool,
    /// Do not ask user confirmation for anything.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Do not diff when installing.
    #[arg(short = 'n', long)]
    pub nodiff: bool,
    /// Remove stale entries from installed directories.
    #[arg(short = 'R', long = "remove-existing")]
    pub remove_existing: bool,
    /// Install to a temporary directory for review.
    #[arg(short = 't', long)]
    pub temp: bool,
    /// Number of concurrent workers.
    #[arg(short = 'w', long, value_name = "nb", default_value = "1")]
    pub workers: String,
    /// Clear the workdir.
    #[arg(short = 'W', long = "workdir-clear")]
    pub workdir_clear: bool,
    pub key: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ImportArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(flatten)]
    pub prof: Prof,
    /// Dry run.
    #[arg(short = 'd', long)]
    pub dry: bool,
    /// Do not ask user confirmation for anything.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Pattern to ignore.
    #[arg(short = 'i', long = "ignore", value_name = "pattern")]
    pub ignore: Vec<String>,
    /// Set the dotfile key.
    #[arg(short = 'K', long = "dkey", value_name = "key")]
    pub dkey: Option<String>,
    /// Link option (nolink|absolute|relative|link_children).
    #[arg(short = 'l', long, value_name = "link")]
    pub link: Option<String>,
    /// Insert a chmod entry in the dotfile with its mode.
    #[arg(short = 'm', long = "preserve-mode")]
    pub preserve_mode: bool,
    /// Import as a different path from actual path.
    #[arg(short = 's', long = "as", value_name = "path")]
    pub import_as: Option<String>,
    /// Associate trans_install key on import.
    #[arg(long, value_name = "key")]
    pub transr: Option<String>,
    /// Apply trans_update key on import.
    #[arg(long, value_name = "key")]
    pub transw: Option<String>,
    #[arg(required = true)]
    pub path: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct CompareArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(flatten)]
    pub prof: Prof,
    /// Path of dotfile to compare.
    #[arg(short = 'C', long = "file", value_name = "file")]
    pub file: Vec<String>,
    /// Pattern to ignore.
    #[arg(short = 'i', long = "ignore", value_name = "pattern")]
    pub ignore: Vec<String>,
    /// Do not show diff but only the files that differ.
    #[arg(short = 'L', long = "file-only")]
    pub file_only: bool,
    /// Number of concurrent workers.
    #[arg(short = 'w', long, value_name = "nb", default_value = "1")]
    pub workers: String,
    /// Ignore files in installed folders that are missing.
    #[arg(short = 'z', long = "ignore-missing")]
    pub ignore_missing: bool,
}

#[derive(Args, Debug, Clone)]
pub struct UpdateArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(flatten)]
    pub prof: Prof,
    /// Dry run.
    #[arg(short = 'd', long)]
    pub dry: bool,
    /// Do not ask user confirmation for anything.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Pattern to ignore.
    #[arg(short = 'i', long = "ignore", value_name = "pattern")]
    pub ignore: Vec<String>,
    /// Treat <path> as a dotfile key.
    #[arg(short = 'k', long)]
    pub key: bool,
    /// Provide a one-liner to manually patch template.
    #[arg(short = 'P', long = "show-patch")]
    pub show_patch: bool,
    /// Number of concurrent workers.
    #[arg(short = 'w', long, value_name = "nb", default_value = "1")]
    pub workers: String,
    /// Ignore files in installed folders that are missing.
    #[arg(short = 'z', long = "ignore-missing")]
    pub ignore_missing: bool,
    pub path: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RemoveArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(flatten)]
    pub prof: Prof,
    /// Dry run.
    #[arg(short = 'd', long)]
    pub dry: bool,
    /// Do not ask user confirmation for anything.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Treat <path> as a dotfile key.
    #[arg(short = 'k', long)]
    pub key: bool,
    pub path: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct UninstallArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(flatten)]
    pub prof: Prof,
    /// Dry run.
    #[arg(short = 'd', long)]
    pub dry: bool,
    /// Do not ask user confirmation for anything.
    #[arg(short = 'f', long)]
    pub force: bool,
    pub key: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct FilesArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(flatten)]
    pub prof: Prof,
    /// Grepable output.
    #[arg(short = 'G', long)]
    pub grepable: bool,
    /// Only template dotfiles.
    #[arg(short = 'T', long)]
    pub template: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DetailArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(flatten)]
    pub prof: Prof,
    pub key: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ProfilesArgs {
    #[command(flatten)]
    pub common: Common,
    /// Grepable output.
    #[arg(short = 'G', long)]
    pub grepable: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Install dotfiles for the profile.
    Install(InstallArgs),
    /// Import dotfiles into the dotpath and the config.
    Import(ImportArgs),
    /// Compare deployed dotfiles with the stored ones.
    Compare(CompareArgs),
    /// Update stored dotfiles from the deployed ones.
    Update(UpdateArgs),
    /// Remove dotfiles from the dotpath and the config.
    Remove(RemoveArgs),
    /// Uninstall dotfiles.
    Uninstall(UninstallArgs),
    /// List dotfiles of a profile.
    Files(FilesArgs),
    /// Show details on the files of dotfiles.
    Detail(DetailArgs),
    /// List profiles.
    Profiles(ProfilesArgs),
    /// Print a default config.
    Gencfg,
}

impl Command {
    fn common(&self) -> Option<&Common> {
        Some(match self {
            Command::Install(a) => &a.common,
            Command::Import(a) => &a.common,
            Command::Compare(a) => &a.common,
            Command::Update(a) => &a.common,
            Command::Remove(a) => &a.common,
            Command::Uninstall(a) => &a.common,
            Command::Files(a) => &a.common,
            Command::Detail(a) => &a.common,
            Command::Profiles(a) => &a.common,
            Command::Gencfg => return None,
        })
    }

    fn profile(&self) -> Option<String> {
        match self {
            Command::Install(a) => a.prof.profile.clone(),
            Command::Import(a) => a.prof.profile.clone(),
            Command::Compare(a) => a.prof.profile.clone(),
            Command::Update(a) => a.prof.profile.clone(),
            Command::Remove(a) => a.prof.profile.clone(),
            Command::Uninstall(a) => a.prof.profile.clone(),
            Command::Files(a) => a.prof.profile.clone(),
            Command::Detail(a) => a.prof.profile.clone(),
            Command::Profiles(_) | Command::Gencfg => None,
        }
    }

    fn dry(&self) -> bool {
        match self {
            Command::Install(a) => a.dry,
            Command::Import(a) => a.dry,
            Command::Update(a) => a.dry,
            Command::Remove(a) => a.dry,
            Command::Uninstall(a) => a.dry,
            _ => false,
        }
    }

    fn force(&self) -> bool {
        match self {
            Command::Install(a) => a.force,
            Command::Import(a) => a.force,
            Command::Update(a) => a.force,
            Command::Remove(a) => a.force,
            Command::Uninstall(a) => a.force,
            _ => false,
        }
    }
}

/// effective options (settings merged with the command line)
pub struct Options {
    pub dry: bool,
    pub safe: bool,
    pub profile: String,
    pub workers: usize,
    pub settings: Settings,
    pub variables: Vars,
    pub dotfiles: Vec<Arc<Dotfile>>,
    pub default_actions: Vec<Action>,
    pub command: Command,

    // install
    pub install_force_action: bool,
    pub install_temporary: bool,
    pub install_keys: Vec<String>,
    pub install_diff: bool,
    pub install_showdiff: bool,
    pub install_clear_workdir: bool,
    pub install_remove_existing: bool,
    // compare
    pub compare_focus: Vec<String>,
    pub compare_ignore: Vec<String>,
    pub compare_fileonly: bool,
    pub ignore_missing_in_dotdrop: bool,
    // import
    pub import_path: Vec<String>,
    pub import_as: Option<String>,
    pub import_mode: bool,
    pub import_ignore: Vec<String>,
    pub import_trans_install: Option<String>,
    pub import_trans_update: Option<String>,
    pub import_force_key: Option<String>,
    pub import_link: LinkType,
    // update
    pub update_path: Vec<String>,
    pub update_iskey: bool,
    pub update_ignore: Vec<String>,
    pub update_showpatch: bool,
    // others
    pub files_templateonly: bool,
    pub files_grepable: bool,
    pub profiles_grepable: bool,
    pub remove_path: Vec<String>,
    pub remove_iskey: bool,
    pub uninstall_key: Vec<String>,
    pub detail_keys: Vec<String>,
}

pub fn default_profile() -> String {
    if let Ok(p) = std::env::var(ENV_PROFILE) {
        return p;
    }
    nix::unistd::gethostname()
        .map(|h| utils::os(h.as_os_str()))
        .unwrap_or_default()
}

/// look for the config file
fn get_config_path(cli_cfg: Option<&str>) -> Option<String> {
    if let Some(c) = cli_cfg.filter(|c| !c.is_empty()) {
        dbg_log!("config from --cfg {c}");
        return Some(utils::expanduser(c));
    }
    for name in [CONFIGFILEYAML, CONFIGFILETOML] {
        if utils::exists(name) {
            dbg_log!("config from current dir {name}");
            return Some(name.to_string());
        }
    }
    let from_env = |name: &str| -> Option<String> {
        let xdg = std::env::var(ENV_XDG).ok()?;
        let path = utils::join(&utils::join(&utils::expanduser(&xdg), NAME), name);
        utils::exists(&path).then_some(path)
    };
    for name in [CONFIGFILEYAML, CONFIGFILETOML] {
        if let Some(p) = from_env(name) {
            return Some(p);
        }
    }
    for name in [CONFIGFILEYAML, CONFIGFILETOML] {
        for dir in [
            utils::expanduser(HOMECFG),
            ETCXDGCFG.to_string(),
            ETCCFG.to_string(),
        ] {
            let path = utils::join(&dir, name);
            if utils::exists(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn parse_workers(arg: &str) -> Result<usize> {
    let val = std::env::var(ENV_WORKERS).unwrap_or_else(|_| arg.to_string());
    val.trim()
        .parse::<usize>()
        .map_err(|_| Error::Options("bad option for --workers".into()))
}

impl Options {
    /// load the config and apply the command line
    pub fn new(command: Command) -> Result<(Self, CfgAggregator)> {
        let common = command
            .common()
            .cloned()
            .expect("gencfg is handled by the caller");
        let debug = common.verbose || std::env::var(ENV_DEBUG).is_ok();
        log::set_debug(debug && std::env::var(ENV_NODEBUG).is_err());
        let dry = command.dry();
        let profile = command.profile().unwrap_or_else(default_profile);

        let confpath = get_config_path(common.cfg.as_deref())
            .ok_or_else(|| Error::Yaml("no config file found".into()))?;
        let confpath = utils::abspath(&confpath);
        if !utils::exists(&confpath) {
            return Err(Error::Yaml(format!("config does not exist \"{confpath}\"")));
        }
        dbg_log!("version: {VERSION}");
        dbg_log!("config file: {confpath}");

        let conf =
            CfgAggregator::new(&confpath, &profile, dry).map_err(|e| Error::Yaml(e.to_string()))?;
        let settings = conf.settings.clone();

        if std::env::var(ENV_NOBANNER).is_err() && settings.banner && !common.no_banner {
            log::log(&banner());
            log::log("");
        }

        let mut o = Options {
            dry,
            safe: !command.force(),
            profile: profile.clone(),
            workers: 1,
            variables: conf.variables.clone(),
            dotfiles: conf.get_dotfiles(Some(&profile)),
            default_actions: conf.default_actions.clone(),
            install_force_action: false,
            install_temporary: false,
            install_keys: Vec::new(),
            install_diff: true,
            install_showdiff: settings.showdiff,
            install_clear_workdir: settings.clear_workdir,
            install_remove_existing: false,
            compare_focus: Vec::new(),
            compare_ignore: Vec::new(),
            compare_fileonly: false,
            ignore_missing_in_dotdrop: settings.ignore_missing_in_dotdrop,
            import_path: Vec::new(),
            import_as: None,
            import_mode: settings.chmod_on_import,
            import_ignore: Vec::new(),
            import_trans_install: None,
            import_trans_update: None,
            import_force_key: None,
            import_link: settings.link_on_import,
            update_path: Vec::new(),
            update_iskey: false,
            update_ignore: Vec::new(),
            update_showpatch: false,
            files_templateonly: false,
            files_grepable: false,
            profiles_grepable: false,
            remove_path: Vec::new(),
            remove_iskey: false,
            uninstall_key: Vec::new(),
            detail_keys: Vec::new(),
            settings,
            command: command.clone(),
        };
        o.apply_args(&command)?;
        Ok((o, conf))
    }

    fn ignores(&self, cli: &[String], settings_ignore: &[String]) -> Vec<String> {
        let mut all: Vec<String> = cli.to_vec();
        all.extend(settings_ignore.iter().cloned());
        all.push(format!("*{BACKUP_SUFFIX}"));
        uniq_list(&all)
    }

    fn apply_args(&mut self, command: &Command) -> Result<()> {
        match command {
            Command::Install(a) => {
                self.workers = parse_workers(&a.workers)?;
                self.install_force_action = a.force_actions;
                self.install_temporary = a.temp;
                self.install_keys = a.key.clone();
                self.install_diff = !a.nodiff;
                self.install_showdiff = self.settings.showdiff || a.showdiff;
                self.install_clear_workdir = a.workdir_clear || self.settings.clear_workdir;
                self.install_remove_existing = a.remove_existing;
            }
            Command::Compare(a) => {
                self.workers = parse_workers(&a.workers)?;
                self.compare_focus = a.file.clone();
                self.compare_ignore = self.ignores(&a.ignore, &self.settings.cmpignore);
                self.compare_fileonly = a.file_only;
                self.ignore_missing_in_dotdrop = self.ignore_missing_in_dotdrop || a.ignore_missing;
            }
            Command::Import(a) => {
                self.import_path = a.path.clone();
                self.import_as = a.import_as.clone();
                self.import_mode = a.preserve_mode || self.settings.chmod_on_import;
                self.import_ignore = self.ignores(&a.ignore, &self.settings.impignore);
                self.import_trans_install = a.transr.clone();
                self.import_trans_update = a.transw.clone();
                self.import_force_key = a.dkey.clone();
                if let Some(link) = &a.link {
                    self.import_link = match link.as_str() {
                        "nolink" | "absolute" | "relative" | "link_children" => {
                            LinkType::parse(link)?
                        }
                        _ => return Err(Error::Options(format!("bad option for --link: {link}"))),
                    };
                }
            }
            Command::Update(a) => {
                self.workers = parse_workers(&a.workers)?;
                self.update_path = a.path.clone();
                self.update_iskey = a.key;
                self.update_ignore = self.ignores(&a.ignore, &self.settings.upignore);
                self.update_showpatch = a.show_patch;
                self.ignore_missing_in_dotdrop = self.ignore_missing_in_dotdrop || a.ignore_missing;
            }
            Command::Files(a) => {
                self.files_templateonly = a.template;
                self.files_grepable = a.grepable;
            }
            Command::Profiles(a) => self.profiles_grepable = a.grepable,
            Command::Remove(a) => {
                self.remove_path = a.path.clone();
                self.remove_iskey = a.key;
            }
            Command::Uninstall(a) => self.uninstall_key = a.key.clone(),
            Command::Detail(a) => self.detail_keys = a.key.clone(),
            Command::Gencfg => {}
        }
        Ok(())
    }

    pub fn install_default_actions(&self, kind: &str) -> Vec<Action> {
        self.default_actions
            .iter()
            .filter(|a| a.kind == kind)
            .cloned()
            .collect()
    }
}
