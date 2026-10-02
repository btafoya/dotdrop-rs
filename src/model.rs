//! Settings, dotfiles, profiles, actions and transformations.

use crate::dbg_log;
use crate::error::{Error, Result, yaml_err};
use crate::linktype::LinkType;
use crate::log;
use crate::templategen::{Templategen, Vars};
use crate::utils;
use serde_yaml_ng::{Mapping, Value};
use std::fmt;
use std::sync::Arc;

pub const ENV_WORKDIR: &str = "DOTDROP_WORKDIR";
pub const DEFAULT_DIFF_CMD: &str = "diff -r -u {0} {1}";
/// special chmod value: keep the mode untouched
pub const CHMOD_PRESERVE: &str = "preserve";

////////////////////////////////////////////////////////////
// yaml accessors
////////////////////////////////////////////////////////////

pub fn vstr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => serde_yaml_ng::to_string(other)
            .unwrap_or_default()
            .trim_end()
            .to_string(),
    }
}

pub fn get_str(m: &Mapping, key: &str) -> Option<String> {
    match m.get(key) {
        None | Some(Value::Null) => None,
        Some(v) => Some(vstr(v)),
    }
}

pub fn get_bool(m: &Mapping, key: &str, default: bool) -> bool {
    match m.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) | None => default,
        Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "true" | "yes" | "1"),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(_) => default,
    }
}

pub fn get_list(m: &Mapping, key: &str) -> Vec<String> {
    match m.get(key) {
        Some(Value::Sequence(s)) => s.iter().map(vstr).collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![vstr(other)],
    }
}

pub fn mapping_to_vars(m: &Mapping) -> Vars {
    m.iter().map(|(k, v)| (vstr(k), v.clone())).collect()
}

////////////////////////////////////////////////////////////
// settings
////////////////////////////////////////////////////////////

#[derive(Debug, Clone)]
pub struct Settings {
    pub backup: bool,
    pub banner: bool,
    pub create: bool,
    pub default_actions: Vec<String>,
    pub dotpath: String,
    pub ignoreempty: bool,
    pub import_actions: Vec<String>,
    pub import_configs: Vec<String>,
    pub import_variables: Vec<String>,
    pub keepdot: bool,
    pub link_dotfile_default: LinkType,
    pub link_on_import: LinkType,
    pub longkey: bool,
    pub upignore: Vec<String>,
    pub cmpignore: Vec<String>,
    pub instignore: Vec<String>,
    pub impignore: Vec<String>,
    pub workdir: String,
    pub showdiff: bool,
    pub minversion: Option<String>,
    pub func_file: Vec<String>,
    pub filter_file: Vec<String>,
    pub diff_command: String,
    pub template_dotfile_default: bool,
    pub ignore_missing_in_dotdrop: bool,
    pub force_chmod: bool,
    pub chmod_on_import: bool,
    pub check_version: bool,
    pub clear_workdir: bool,
    pub compare_workdir: bool,
    pub key_prefix: bool,
    pub key_separator: String,
}

pub const SETTINGS_KEYS: [&str; 32] = [
    "backup",
    "banner",
    "create",
    "default_actions",
    "dotpath",
    "ignoreempty",
    "import_actions",
    "import_configs",
    "import_variables",
    "keepdot",
    "link_dotfile_default",
    "link_on_import",
    "longkey",
    "upignore",
    "cmpignore",
    "instignore",
    "impignore",
    "workdir",
    "showdiff",
    "minversion",
    "func_file",
    "filter_file",
    "diff_command",
    "template_dotfile_default",
    "ignore_missing_in_dotdrop",
    "force_chmod",
    "chmod_on_import",
    "check_version",
    "clear_workdir",
    "compare_workdir",
    "key_prefix",
    "key_separator",
];

impl Default for Settings {
    fn default() -> Self {
        Self {
            backup: true,
            banner: true,
            create: true,
            default_actions: Vec::new(),
            dotpath: "dotfiles".into(),
            ignoreempty: false,
            import_actions: Vec::new(),
            import_configs: Vec::new(),
            import_variables: Vec::new(),
            keepdot: false,
            link_dotfile_default: LinkType::NoLink,
            link_on_import: LinkType::NoLink,
            longkey: false,
            upignore: Vec::new(),
            cmpignore: Vec::new(),
            instignore: Vec::new(),
            impignore: Vec::new(),
            workdir: "~/.config/dotdrop".into(),
            showdiff: false,
            minversion: None,
            func_file: Vec::new(),
            filter_file: Vec::new(),
            diff_command: DEFAULT_DIFF_CMD.into(),
            template_dotfile_default: true,
            ignore_missing_in_dotdrop: false,
            force_chmod: false,
            chmod_on_import: false,
            check_version: false,
            clear_workdir: false,
            compare_workdir: false,
            key_prefix: true,
            key_separator: "_".into(),
        }
    }
}

impl Settings {
    /// build from the "config" block (defaults for missing entries)
    pub fn from_mapping(m: &Mapping) -> Result<Self> {
        for k in m.keys() {
            let k = vstr(k);
            if !SETTINGS_KEYS.contains(&k.as_str()) {
                return yaml_err(format!("config content error: unknown setting \"{k}\""));
            }
        }
        let mut d = Self::default();
        if let Ok(w) = std::env::var(ENV_WORKDIR) {
            // the environment only provides the default value of the config entry
            d.workdir = w;
        }
        let b = |k: &str, dv: bool| get_bool(m, k, dv);
        let link = |k: &str, dv: LinkType| -> Result<LinkType> {
            match get_str(m, k) {
                Some(s) => LinkType::parse(&s)
                    .map_err(|_| Error::Yaml(format!("config content error: bad link value: {s}"))),
                None => Ok(dv),
            }
        };
        let s = Self {
            backup: b("backup", d.backup),
            banner: b("banner", d.banner),
            create: b("create", d.create),
            default_actions: get_list(m, "default_actions"),
            dotpath: get_str(m, "dotpath").unwrap_or(d.dotpath),
            ignoreempty: b("ignoreempty", d.ignoreempty),
            import_actions: get_list(m, "import_actions"),
            import_configs: get_list(m, "import_configs"),
            import_variables: get_list(m, "import_variables"),
            keepdot: b("keepdot", d.keepdot),
            link_dotfile_default: link("link_dotfile_default", d.link_dotfile_default)?,
            link_on_import: link("link_on_import", d.link_on_import)?,
            longkey: b("longkey", d.longkey),
            upignore: get_list(m, "upignore"),
            cmpignore: get_list(m, "cmpignore"),
            instignore: get_list(m, "instignore"),
            impignore: get_list(m, "impignore"),
            workdir: get_str(m, "workdir").unwrap_or(d.workdir),
            showdiff: b("showdiff", d.showdiff),
            minversion: get_str(m, "minversion"),
            func_file: get_list(m, "func_file"),
            filter_file: get_list(m, "filter_file"),
            diff_command: get_str(m, "diff_command").unwrap_or(d.diff_command),
            template_dotfile_default: b("template_dotfile_default", d.template_dotfile_default),
            ignore_missing_in_dotdrop: b("ignore_missing_in_dotdrop", d.ignore_missing_in_dotdrop),
            force_chmod: b("force_chmod", d.force_chmod),
            chmod_on_import: b("chmod_on_import", d.chmod_on_import),
            check_version: b("check_version", d.check_version),
            clear_workdir: b("clear_workdir", d.clear_workdir),
            compare_workdir: b("compare_workdir", d.compare_workdir),
            key_prefix: b("key_prefix", d.key_prefix),
            key_separator: get_str(m, "key_separator").unwrap_or(d.key_separator),
        };
        if m.contains_key("diff_command") && get_str(m, "diff_command").is_none() {
            return yaml_err("bad diff_command: None");
        }
        if !utils::is_bin_in_path(&s.diff_command) {
            return yaml_err(format!("bad diff_command: {}", s.diff_command));
        }
        Ok(s)
    }
}

////////////////////////////////////////////////////////////
// actions / transformations
////////////////////////////////////////////////////////////

/// minimal implementation of python's `str.format` with positional args
fn py_format(fmt: &str, args: &[String]) -> std::result::Result<String, String> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut auto = 0usize;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let end = chars[i..]
                    .iter()
                    .position(|c| *c == '}')
                    .ok_or("expected '}' before end of string")?;
                let field: String = chars[i + 1..i + end].iter().collect();
                let name = field.split(['!', ':']).next().unwrap_or("");
                let idx = if name.is_empty() {
                    auto += 1;
                    auto - 1
                } else if let Ok(n) = name.parse::<usize>() {
                    n
                } else {
                    return Err(format!("KeyError: '{name}'"));
                };
                match args.get(idx) {
                    Some(a) => out.push_str(a),
                    None => {
                        return Err(format!("IndexError: Replacement index {idx} out of range"));
                    }
                }
                i += end + 1;
            }
            '}' => return Err("Single '}' encountered in format string".into()),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// execute a command (action or transformation) through the shell
fn execute_cmd(
    descr: &str,
    key: &str,
    action: &str,
    args: &[String],
    templater: Option<&Templategen>,
) -> bool {
    let silent = key.starts_with('_');
    let mut act = action.to_string();
    let mut args = args.to_vec();
    if let Some(t) = templater {
        match t.generate_string(action) {
            Ok(a) => {
                dbg_log!("{descr}:");
                dbg_log!("  - raw       \"{action}\"");
                dbg_log!("  - templated \"{a}\"");
                act = a;
            }
            Err(e) => {
                log::warn(format!("undefined variable for {descr}: \"{e}\""));
                return false;
            }
        }
        let mut new = Vec::new();
        for a in &args {
            match t.generate_string(a) {
                Ok(n) => new.push(n),
                Err(e) => {
                    log::warn(format!("undefined arguments for {descr}: {e}"));
                    return false;
                }
            }
        }
        args = new;
    }
    for (cnt, a) in args.iter().enumerate() {
        dbg_log!("\targs[{cnt}]: {a}");
    }
    let cmd = match py_format(&act, &args) {
        Ok(c) => c,
        Err(e) => {
            log::warn(format!(
                "error for {descr}: \"{act}\" with \"{args:?}\": {e}"
            ));
            return false;
        }
    };
    if silent {
        log::sub(&format!("executing silent action \"{key}\""));
    } else {
        dbg_log!("action cmd: \"{cmd}\"");
        log::sub(&format!("executing \"{cmd}\""));
    }
    let ret = utils::shell_call(&cmd);
    if ret != 0 {
        log::warn(format!("{descr} returned code {ret}"));
    }
    ret == 0
}

#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub key: String,
    pub kind: String,
    pub action: String,
    pub args: Vec<String>,
}

pub const ACTION_PRE: &str = "pre";
pub const ACTION_POST: &str = "post";

impl Action {
    pub fn new(key: &str, kind: &str, action: &str) -> Self {
        Self {
            key: key.into(),
            kind: kind.into(),
            action: action.into(),
            args: Vec::new(),
        }
    }

    pub fn with_args(&self, args: Vec<String>) -> Self {
        Self {
            args,
            ..self.clone()
        }
    }

    pub fn execute(&self, templater: Option<&Templategen>) -> bool {
        execute_cmd("action", &self.key, &self.action, &self.args, templater)
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: [{}] \"{}\"", self.key, self.kind, self.action)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transform {
    pub key: String,
    pub action: String,
    pub args: Vec<String>,
}

impl Transform {
    pub fn new(key: &str, action: &str) -> Self {
        Self {
            key: key.into(),
            action: action.into(),
            args: Vec::new(),
        }
    }

    pub fn with_args(&self, args: Vec<String>) -> Self {
        Self {
            args,
            ..self.clone()
        }
    }

    /// execute the transformation with {0} the file to transform
    /// and {1} the result file
    pub fn transform(&self, arg0: &str, arg1: &str, templater: Option<&Templategen>) -> bool {
        if utils::exists(arg1) {
            log::warn(format!(
                "transformation \"{}\": destination exists: {arg1}",
                self.key
            ));
            return false;
        }
        let mut args = vec![arg0.to_string(), arg1.to_string()];
        args.extend(self.args.iter().cloned());
        execute_cmd("transformation", &self.key, &self.action, &args, templater)
    }
}

impl fmt::Display for Transform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key:{} -> \"{}\"", self.key, self.action)
    }
}

////////////////////////////////////////////////////////////
// dotfile
////////////////////////////////////////////////////////////

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chmod {
    Mode(u32),
    Preserve,
}

impl fmt::Display for Chmod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Chmod::Mode(m) => write!(f, "{m:o}"),
            Chmod::Preserve => f.write_str(CHMOD_PRESERVE),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dotfile {
    pub key: String,
    pub dst: String,
    pub src: String,
    pub actions: Vec<Action>,
    pub trans_install: Option<Transform>,
    pub trans_update: Option<Transform>,
    pub link: LinkType,
    pub noempty: bool,
    pub cmpignore: Vec<String>,
    pub upignore: Vec<String>,
    pub instignore: Vec<String>,
    pub template: bool,
    pub chmod: Option<Chmod>,
    pub ignore_missing_in_dotdrop: bool,
    pub dir_as_block: Vec<String>,
}

impl Dotfile {
    pub fn dotfile_variables(&self) -> Vars {
        let mut v = Vars::new();
        v.insert("_dotfile_abs_src".into(), Value::String(self.src.clone()));
        v.insert("_dotfile_abs_dst".into(), Value::String(self.dst.clone()));
        v.insert("_dotfile_key".into(), Value::String(self.key.clone()));
        v.insert("_dotfile_link".into(), Value::String(self.link.to_string()));
        v
    }

    pub fn pre_actions(&self) -> Vec<Action> {
        self.actions
            .iter()
            .filter(|a| a.kind == ACTION_PRE)
            .cloned()
            .collect()
    }

    pub fn post_actions(&self) -> Vec<Action> {
        self.actions
            .iter()
            .filter(|a| a.kind == ACTION_POST)
            .cloned()
            .collect()
    }

    /// extended dotfile to str
    pub fn prt(&self) -> String {
        let ind = "  ";
        let mut out = format!("dotfile: \"{}\"", self.key);
        out += &format!("\n{ind}src: \"{}\"", self.src);
        out += &format!("\n{ind}dst: \"{}\"", self.dst);
        out += &format!("\n{ind}link: \"{}\"", self.link);
        out += &format!(
            "\n{ind}template: \"{}\"",
            if self.template { "True" } else { "False" }
        );
        if let Some(c) = &self.chmod {
            out += &format!("\n{ind}chmod: \"{c}\"");
        }
        if !self.dir_as_block.is_empty() {
            out += &format!("\n{ind}dir_as_block: \"{:?}\"", self.dir_as_block);
        }
        out += &format!("\n{ind}pre-action:");
        for a in self.pre_actions() {
            out += &format!("\n{ind}{ind}- {a}");
        }
        out += &format!("\n{ind}post-action:");
        for a in self.post_actions() {
            out += &format!("\n{ind}{ind}- {a}");
        }
        out += &format!("\n{ind}trans_install:");
        if let Some(t) = &self.trans_install {
            out += &format!("\n{ind}{ind}- {t}");
        }
        out += &format!("\n{ind}trans_update:");
        if let Some(t) = &self.trans_update {
            out += &format!("\n{ind}{ind}- {t}");
        }
        out
    }
}

impl fmt::Display for Dotfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "key:\"{}\", src:\"{}\", dst:\"{}\", link:\"{}\", template:{}",
            self.key,
            self.src,
            self.dst,
            self.link,
            if self.template { "True" } else { "False" }
        )?;
        if let Some(t) = &self.trans_install {
            write!(f, ", trans_install:{t}")?;
        }
        if let Some(t) = &self.trans_update {
            write!(f, ", trans_update:{t}")?;
        }
        if let Some(c) = &self.chmod {
            write!(f, ", chmod:{c}")?;
        }
        if !self.dir_as_block.is_empty() {
            write!(f, ", dir_as_block:{:?}", self.dir_as_block)?;
        }
        Ok(())
    }
}

////////////////////////////////////////////////////////////
// profile
////////////////////////////////////////////////////////////

#[derive(Debug, Clone)]
pub struct Profile {
    pub key: String,
    pub actions: Vec<Action>,
    pub dotfiles: Vec<Arc<Dotfile>>,
    pub description: Option<String>,
    pub group: Option<String>,
}

impl Profile {
    /// hidden profiles have their key prefixed with an underscore
    pub fn hidden(&self) -> bool {
        self.key.starts_with('_')
    }

    pub fn pre_actions(&self) -> Vec<Action> {
        self.actions
            .iter()
            .filter(|a| a.kind == ACTION_PRE)
            .cloned()
            .collect()
    }

    pub fn post_actions(&self) -> Vec<Action> {
        self.actions
            .iter()
            .filter(|a| a.kind == ACTION_POST)
            .cloned()
            .collect()
    }
}
