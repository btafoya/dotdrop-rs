//! Higher level of the config: typed dotfiles, profiles, actions and variables.

use crate::cfg_yaml::{self as cy, CfgYaml, KEY_ALL};
use crate::dbg_log;
use crate::error::{Error, Result};
use crate::linktype::LinkType;
use crate::log;
use crate::model::{
    Action, CHMOD_PRESERVE, Chmod, Dotfile, Profile, Settings, Transform, get_bool, get_list,
    get_str, vstr,
};
use crate::templategen::Vars;
use crate::utils;
use regex::Regex;
use serde_yaml_ng::{Mapping, Value};
use std::sync::Arc;

const TILD: &str = "~";
const FILE_PREFIX: &str = "f";
const DIR_PREFIX: &str = "d";

const DOTFILE_KEYS: [&str; 14] = [
    "src",
    "dst",
    "link",
    "ignoreempty",
    "template",
    "chmod",
    "actions",
    "trans_install",
    "trans_update",
    "cmpignore",
    "upignore",
    "instignore",
    "ignore_missing_in_dotdrop",
    "dir_as_block",
];

const PROFILE_KEYS: [&str; 8] = [
    "dotfiles",
    "variables",
    "dynvariables",
    "actions",
    "description",
    "group",
    "include",
    "import",
];

fn yaml_ok() -> Regex {
    Regex::new(r"[^0-9a-zA-Z.\-_+]+").expect("static regex")
}

/// posix `shlex.split`
pub fn shlex_split(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(n) => cur.push(n),
                    None => return Err(Error::Config("No escaped character".into())),
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(n) => cur.push(n),
                        None => return Err(Error::Config("No closing quotation".into())),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(n @ ('"' | '\\')) => cur.push(n),
                            Some(n) => {
                                cur.push('\\');
                                cur.push(n);
                            }
                            None => return Err(Error::Config("No closing quotation".into())),
                        },
                        Some(n) => cur.push(n),
                        None => return Err(Error::Config("No closing quotation".into())),
                    }
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

pub struct CfgAggregator {
    path: String,
    profile_key: String,
    dry: bool,
    cfgyaml: CfgYaml,
    pub settings: Settings,
    pub default_actions: Vec<Action>,
    pub dotfiles: Vec<Arc<Dotfile>>,
    pub profiles: Vec<Profile>,
    actions: Vec<Action>,
    trans_install: Vec<Transform>,
    trans_update: Vec<Transform>,
    pub variables: Vars,
    key_separator: String,
}

impl CfgAggregator {
    pub fn new(path: &str, profile_key: &str, dry: bool) -> Result<Self> {
        let cfgyaml = CfgYaml::new(path, Some(profile_key), Vec::new(), false, None, true)?;
        let mut me = Self {
            path: path.to_string(),
            profile_key: profile_key.to_string(),
            dry,
            settings: cfgyaml.settings.clone(),
            cfgyaml,
            default_actions: Vec::new(),
            dotfiles: Vec::new(),
            profiles: Vec::new(),
            actions: Vec::new(),
            trans_install: Vec::new(),
            trans_update: Vec::new(),
            variables: Vars::new(),
            key_separator: "_".into(),
        };
        me.load_from_yaml()?;
        if me.settings.workdir.is_empty() {
            return Err(Error::Undefined("\"workdir\" is undefined".into()));
        }
        Ok(me)
    }

    ////////////////////////////////////////////////////////
    // public methods
    ////////////////////////////////////////////////////////

    pub fn del_dotfile(&mut self, dotfile: &Dotfile) -> bool {
        self.cfgyaml.del_dotfile(&dotfile.key)
    }

    pub fn del_dotfile_from_profile(&mut self, dotfile: &Dotfile, profile: &Profile) -> bool {
        self.cfgyaml
            .del_dotfile_from_profile(&dotfile.key, &profile.key)
    }

    /// import a new dotfile
    #[allow(clippy::too_many_arguments)]
    pub fn new_dotfile(
        &mut self,
        src: &str,
        dst: &str,
        link: LinkType,
        chmod: Option<u32>,
        trans_install: Option<&Transform>,
        trans_update: Option<&Transform>,
        forcekey: Option<&str>,
    ) -> Result<bool> {
        let dst = self.path_to_dotfile_dst(dst);
        let existing = self.get_dotfile_by_src_dst(src, &dst);
        let key = match existing {
            Some(d) => d.key.clone(),
            None => {
                let Some(key) = self.get_new_dotfile_key(&dst, forcekey) else {
                    return Ok(false);
                };
                dbg_log!("new dotfile key: {key}");
                if !self.cfgyaml.add_dotfile(
                    &key,
                    src,
                    &dst,
                    link,
                    chmod,
                    trans_install.map(|t| t.key.as_str()),
                    trans_update.map(|t| t.key.as_str()),
                ) {
                    return Ok(false);
                }
                key
            }
        };
        let mut ret = true;
        if self.profile_key != KEY_ALL {
            ret = self
                .cfgyaml
                .add_dotfile_to_profile(&key, &self.profile_key)?;
            if ret {
                dbg_log!("new dotfile {key} to profile {}", self.profile_key);
            }
        }
        if ret {
            self.save_and_reload()?;
        }
        Ok(ret)
    }

    pub fn update_dotfile(&mut self, key: &str, chmod: Option<u32>) -> Result<bool> {
        let ret = self.cfgyaml.update_dotfile(key, chmod);
        if ret {
            self.save_and_reload()?;
        }
        Ok(ret)
    }

    /// normalize the path to match dotfile dst
    pub fn path_to_dotfile_dst(&self, path: &str) -> String {
        let path = Self::norm_path(path);
        let home = format!("{}/", utils::home());
        match path.strip_prefix(&home) {
            Some(rest) => utils::join(TILD, rest),
            None => path,
        }
    }

    /// dotfiles by dst (on filesystem)
    pub fn get_dotfile_by_dst(&self, dst: &str, profile_key: Option<&str>) -> Vec<Arc<Dotfile>> {
        let dst = Self::norm_path(dst);
        let dfs = match profile_key {
            Some(p) => self.get_dotfiles(Some(p)),
            None => self.dotfiles.clone(),
        };
        dfs.into_iter()
            .filter(|d| Self::norm_path(&d.dst) == dst)
            .collect()
    }

    pub fn get_dotfile_by_src_dst(&self, src: &str, dst: &str) -> Option<Arc<Dotfile>> {
        let src = if utils::is_abs(src) {
            src.to_string()
        } else {
            match self.cfgyaml.resolve_dotfile_src(src, None) {
                Ok(s) => s,
                Err(e) => {
                    log::err(format!("unable to resolve {src}: {e}"));
                    return None;
                }
            }
        };
        self.get_dotfile_by_dst(dst, None).into_iter().find(|d| {
            self.cfgyaml
                .resolve_dotfile_src(&d.src, None)
                .map(|dsrc| dsrc == src)
                .unwrap_or(false)
        })
    }

    pub fn save(&mut self) -> Result<bool> {
        if self.dry {
            return Ok(true);
        }
        self.cfgyaml.save()
    }

    pub fn dump(&self) -> Result<String> {
        self.cfgyaml.dump()
    }

    pub fn get_profile(&self, key: Option<&str>) -> Option<&Profile> {
        let pro = key.unwrap_or(&self.profile_key);
        self.profiles.iter().find(|p| p.key == pro)
    }

    pub fn get_profiles_by_dotfile_key(&self, key: &str) -> Vec<&Profile> {
        self.profiles
            .iter()
            .filter(|p| p.dotfiles.iter().any(|d| d.key == key))
            .collect()
    }

    /// dotfiles of the current profile, of `profile_key` or all if it is ALL
    pub fn get_dotfiles(&self, profile_key: Option<&str>) -> Vec<Arc<Dotfile>> {
        if profile_key == Some(KEY_ALL) {
            return self.dotfiles.clone();
        }
        self.get_profile(profile_key)
            .map(|p| p.dotfiles.clone())
            .unwrap_or_default()
    }

    pub fn get_dotfile(&self, key: &str, profile_key: Option<&str>) -> Option<Arc<Dotfile>> {
        let dfs = match profile_key {
            Some(p) => self.get_profile(Some(p))?.dotfiles.clone(),
            None => self.dotfiles.clone(),
        };
        dfs.into_iter().find(|d| d.key == key)
    }

    pub fn get_trans_install(&self, key: &str) -> Option<Transform> {
        self.trans_install.iter().find(|t| t.key == key).cloned()
    }

    pub fn get_trans_update(&self, key: &str) -> Option<Transform> {
        self.trans_update.iter().find(|t| t.key == key).cloned()
    }

    ////////////////////////////////////////////////////////
    // parsing
    ////////////////////////////////////////////////////////

    fn load_from_yaml(&mut self) -> Result<()> {
        dbg_log!("parsing cfgyaml into cfg_aggregator");
        self.settings = self.cfgyaml.settings.clone();
        // the environment overrides the workdir of the config for the effective settings
        if let Ok(w) = std::env::var(crate::model::ENV_WORKDIR) {
            self.settings.workdir = w;
        }
        self.key_separator = yaml_ok()
            .replace_all(&self.settings.key_separator, "_")
            .into_owned();

        // actions and transformations first, they are referenced by keys
        self.actions = self
            .cfgyaml
            .actions
            .iter()
            .map(|(key, (kind, action))| Action::new(key, kind, action))
            .collect();
        self.trans_install = self
            .cfgyaml
            .trans_install
            .iter()
            .map(|(key, a)| Transform::new(key, a))
            .collect();
        self.trans_update = self
            .cfgyaml
            .trans_update
            .iter()
            .map(|(key, a)| Transform::new(key, a))
            .collect();

        self.variables = self.cfgyaml.variables.clone();
        self.enrich_variables();

        // dotfiles
        let mut dotfiles = Vec::new();
        for (key, entry) in &self.cfgyaml.dotfiles {
            dotfiles.push(Arc::new(self.parse_dotfile(key, entry)?));
        }
        self.dotfiles = dotfiles;

        // profiles
        let mut profiles = Vec::new();
        for (key, entry) in &self.cfgyaml.profiles {
            profiles.push(self.parse_profile(key, entry)?);
        }
        self.profiles = profiles;

        self.default_actions = self
            .settings
            .default_actions
            .iter()
            .map(|k| self.action_by_key(k, "default_actions"))
            .collect::<Result<_>>()?;
        dbg_log!("default actions: {:?}", self.default_actions);
        dbg_log!("done parsing cfgyaml into cfg_aggregator");
        Ok(())
    }

    fn container_err(container: &str, keys: &str, key: &str) -> Error {
        let err = format!("{container} does not contain a {keys} entry named {key}");
        log::err(&err);
        Error::Config(err)
    }

    /// action by key with the optional arguments ("key arg1 arg2")
    fn action_by_key(&self, key: &str, container: &str) -> Result<Action> {
        let fields = shlex_split(key)?;
        let name = fields.first().cloned().unwrap_or_default();
        let action = self
            .actions
            .iter()
            .find(|a| a.key == name)
            .ok_or_else(|| Self::container_err(container, "actions", &name))?;
        if fields.len() > 1 {
            dbg_log!("action with parm: {name} and {:?}", &fields[1..]);
            return Ok(action.with_args(fields[1..].to_vec()));
        }
        Ok(action.clone())
    }

    fn trans_by_key(
        &self,
        key: &str,
        list: &[Transform],
        what: &str,
        container: &str,
    ) -> Result<Transform> {
        let fields = shlex_split(key)?;
        let name = fields.first().cloned().unwrap_or_default();
        let trans = list
            .iter()
            .find(|t| t.key == name)
            .ok_or_else(|| Self::container_err(container, what, &name))?;
        if fields.len() > 1 {
            return Ok(trans.with_args(fields[1..].to_vec()));
        }
        Ok(trans.clone())
    }

    fn parse_dotfile(&self, key: &str, entry: &Mapping) -> Result<Dotfile> {
        for kk in entry.keys() {
            let kk = vstr(kk);
            if !DOTFILE_KEYS.contains(&kk.as_str()) {
                return Err(Error::Yaml(format!(
                    "config content error: dotfile \"{key}\": unknown entry \"{kk}\""
                )));
            }
        }
        let container = format!("key:\"{key}\"");
        let actions = get_list(entry, "actions")
            .iter()
            .map(|a| self.action_by_key(a, &container))
            .collect::<Result<Vec<_>>>()?;
        let mut trans_install = match get_str(entry, cy::KEY_TRANS_INSTALL) {
            Some(t) if !t.is_empty() => {
                Some(self.trans_by_key(&t, &self.trans_install, "trans_install", &container)?)
            }
            _ => None,
        };
        let mut trans_update = match get_str(entry, cy::KEY_TRANS_UPDATE) {
            Some(t) if !t.is_empty() => {
                Some(self.trans_by_key(&t, &self.trans_update, "trans_update", &container)?)
            }
            _ => None,
        };
        let link = LinkType::parse(&get_str(entry, "link").unwrap_or_else(|| "nolink".into()))?;
        if link != LinkType::NoLink && (trans_install.is_some() || trans_update.is_some()) {
            log::warn(format!(
                "[{key}] transformations disabled because dotfile is linked"
            ));
            trans_install = None;
            trans_update = None;
        }
        let chmod = match entry.get("chmod") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => n.as_u64().map(|m| Chmod::Mode(m as u32)),
            Some(Value::String(s)) if s == CHMOD_PRESERVE => Some(Chmod::Preserve),
            Some(other) => {
                return Err(Error::Yaml(format!(
                    "config content error: bad format for chmod: {}",
                    vstr(other)
                )));
            }
        };
        Ok(Dotfile {
            key: key.to_string(),
            dst: get_str(entry, "dst").unwrap_or_default(),
            src: get_str(entry, "src").unwrap_or_default(),
            actions,
            trans_install,
            trans_update,
            link,
            noempty: get_bool(entry, "ignoreempty", false),
            cmpignore: get_list(entry, "cmpignore"),
            upignore: get_list(entry, "upignore"),
            instignore: get_list(entry, "instignore"),
            template: get_bool(entry, "template", true),
            chmod,
            ignore_missing_in_dotdrop: get_bool(entry, "ignore_missing_in_dotdrop", false),
            dir_as_block: get_list(entry, "dir_as_block"),
        })
    }

    fn parse_profile(&self, key: &str, entry: &Mapping) -> Result<Profile> {
        for kk in entry.keys() {
            let kk = vstr(kk);
            if !PROFILE_KEYS.contains(&kk.as_str()) {
                return Err(Error::Yaml(format!(
                    "config content error: profile \"{key}\": unknown entry \"{kk}\""
                )));
            }
        }
        let container = format!("key:\"{key}\"");
        let mut dotfiles = Vec::new();
        for dk in get_list(entry, "dotfiles") {
            let d = self
                .dotfiles
                .iter()
                .find(|d| d.key == dk)
                .ok_or_else(|| Self::container_err(&container, "dotfiles", &dk))?;
            dotfiles.push(d.clone());
        }
        let actions = get_list(entry, "actions")
            .iter()
            .map(|a| self.action_by_key(a, &container))
            .collect::<Result<Vec<_>>>()?;
        Ok(Profile {
            key: key.to_string(),
            actions,
            dotfiles,
            description: get_str(entry, "description"),
            group: get_str(entry, "group"),
        })
    }

    /// enrich available variables (os, release, distro_*)
    fn enrich_variables(&mut self) {
        let release = nix::sys::utsname::uname()
            .map(|u| u.release().to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let os = match std::env::consts::OS {
            "macos" => "darwin".to_string(),
            o => o.to_string(),
        };
        let entries = [
            ("os", os),
            ("release", release),
            ("distro_id", utils::os_release_field("ID").to_lowercase()),
            (
                "distro_version",
                utils::os_release_field("VERSION_ID").to_lowercase(),
            ),
            (
                "distro_like",
                utils::os_release_field("ID_LIKE").to_lowercase(),
            ),
        ];
        for (name, val) in entries {
            if !self.variables.contains_key(name) {
                dbg_log!("enrich variables with {name}={val}");
                self.variables.insert(name.to_string(), Value::String(val));
            }
        }
    }

    ////////////////////////////////////////////////////////
    // dotfile key
    ////////////////////////////////////////////////////////

    fn get_new_dotfile_key(&self, dst: &str, forcekey: Option<&str>) -> Option<String> {
        let existing = self.cfgyaml.all_dotfile_keys();
        if let Some(fk) = forcekey.filter(|k| !k.is_empty()) {
            let key = Self::norm_key_elem(fk);
            return Some(self.uniq_key(&key, &existing));
        }
        let path = utils::expanduser(dst);
        if self.settings.longkey {
            return Some(self.get_long_key(&path, &existing));
        }
        Some(self.get_short_key(&path, &existing))
    }

    fn norm_key_elem(elem: &str) -> String {
        let elem = elem.trim_start_matches('.').replace(' ', "-");
        yaml_ok().replace_all(&elem, "_").to_lowercase()
    }

    fn key_prefix(&self, path: &str) -> Vec<String> {
        if !self.settings.key_prefix {
            return Vec::new();
        }
        if utils::is_dir(path) {
            vec![DIR_PREFIX.to_string()]
        } else {
            vec![FILE_PREFIX.to_string()]
        }
    }

    fn get_long_key(&self, path: &str, keys: &[String]) -> String {
        let dirs = Self::split_path_for_key(path);
        let mut parts = self.key_prefix(path);
        parts.extend(dirs);
        self.uniq_key(&parts.join(&self.key_separator), keys)
    }

    fn get_short_key(&self, path: &str, keys: &[String]) -> String {
        let mut dirs = Self::split_path_for_key(path);
        dirs.reverse();
        let prefix = self.key_prefix(path);
        let mut entries: Vec<String> = Vec::new();
        let mut key = String::new();
        for dri in dirs {
            entries.insert(0, dri);
            let mut parts = prefix.clone();
            parts.extend(entries.iter().cloned());
            key = parts.join(&self.key_separator);
            if !keys.contains(&key) {
                return key;
            }
        }
        self.uniq_key(&key, keys)
    }

    fn uniq_key(&self, key: &str, keys: &[String]) -> String {
        let mut newkey = key.to_string();
        let mut cnt = 1;
        while keys.contains(&newkey) {
            newkey = format!("{key}{}{cnt}", self.key_separator);
            cnt += 1;
        }
        newkey
    }

    /// list of path elements, excluded home path
    fn split_path_for_key(path: &str) -> Vec<String> {
        let mut path = utils::strip_home(path);
        let mut dirs = Vec::new();
        loop {
            let (head, file) = utils::split(&path);
            dirs.push(file.clone());
            path = head;
            if path.is_empty() || file.is_empty() {
                break;
            }
        }
        dirs.reverse();
        dirs.into_iter()
            .filter(|d| !d.is_empty())
            .map(|d| Self::norm_key_elem(&d))
            .collect()
    }

    ////////////////////////////////////////////////////////
    // helpers
    ////////////////////////////////////////////////////////

    fn save_and_reload(&mut self) -> Result<()> {
        if self.dry {
            return Ok(());
        }
        self.save()?;
        dbg_log!("reloading config");
        let was = log::is_debug();
        log::set_debug(false);
        let res = CfgYaml::new(
            &self.path,
            Some(&self.profile_key),
            Vec::new(),
            true,
            None,
            true,
        )
        .map_err(|e| Error::Yaml(e.to_string()))
        .and_then(|y| {
            self.cfgyaml = y;
            self.load_from_yaml()
        });
        log::set_debug(was);
        res
    }

    fn norm_path(path: &str) -> String {
        if path.is_empty() {
            return String::new();
        }
        utils::abspath(&utils::expandvars(&utils::expanduser(path)))
    }
}
