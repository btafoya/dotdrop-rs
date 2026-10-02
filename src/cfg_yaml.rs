//! Lower level of the config file handling (yaml/toml documents, variables,
//! includes, imports).

use crate::dbg_log;
use crate::error::{Error, Result, yaml_err};
use crate::linktype::LinkType;
use crate::log;
use crate::model::{CHMOD_PRESERVE, Settings, get_list, get_str, mapping_to_vars, vstr};
use crate::templategen::{Templategen, Vars};
use crate::utils::{self, VERSION};
use indexmap::IndexMap;
use serde_yaml_ng::{Mapping, Number, Value};
use std::fs;
use std::sync::{Arc, Mutex};

pub const KEY_SETTINGS: &str = "config";
pub const KEY_DOTFILES: &str = "dotfiles";
pub const KEY_PROFILES: &str = "profiles";
pub const KEY_ACTIONS: &str = "actions";
pub const KEY_TRANS_INSTALL: &str = "trans_install";
pub const KEY_TRANS_UPDATE: &str = "trans_update";
pub const KEY_VARIABLES: &str = "variables";
pub const KEY_DVARIABLES: &str = "dynvariables";
pub const KEY_UVARIABLES: &str = "uservariables";
pub const KEY_ALL: &str = "ALL";

const OLD_KEY_TRANS: &str = "trans";
const OLD_KEY_TRANS_R: &str = "trans_read";
const OLD_KEY_TRANS_W: &str = "trans_write";

const ACTION_PRE: &str = "pre";
const ACTION_POST: &str = "post";

pub const KEY_DOTFILE_SRC: &str = "src";
pub const KEY_DOTFILE_DST: &str = "dst";
pub const KEY_DOTFILE_LINK: &str = "link";
pub const KEY_DOTFILE_NOEMPTY: &str = "ignoreempty";
pub const KEY_DOTFILE_TEMPLATE: &str = "template";
pub const KEY_DOTFILE_CHMOD: &str = "chmod";

pub const KEY_PROFILE_DOTFILES: &str = "dotfiles";
pub const KEY_PROFILE_INCLUDE: &str = "include";
pub const KEY_PROFILE_VARIABLES: &str = "variables";
pub const KEY_PROFILE_DVARIABLES: &str = "dynvariables";
pub const KEY_PROFILE_ACTIONS: &str = "actions";
pub const KEY_PROFILE_DESCRIPTION: &str = "description";
pub const KEY_PROFILE_GROUP: &str = "group";
pub const KEY_IMPORT_PROFILE_DFS: &str = "import";

const IMPORT_SEP: char = ':';
const IMPORT_IGNORE_KEY: &str = "optional";

/// recursion guard when resolving self-referencing templates
const MAX_TEMPLATE_PASSES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Yaml,
    Toml,
}

/// shared list of imported config paths
pub type ImportedConfigs = Arc<Mutex<Vec<String>>>;

pub struct CfgYaml {
    path: String,
    profile: Option<String>,
    inc_profiles: Vec<String>,
    reloading: bool,
    format: Format,
    dirty: bool,
    dirty_deprecated: bool,
    profilevarskeys: Vec<String>,
    pub imported_configs: ImportedConfigs,

    yaml: Mapping,

    pub settings: Settings,
    pub dotfiles: IndexMap<String, Mapping>,
    pub profiles: IndexMap<String, Mapping>,
    /// action key -> (kind, command)
    pub actions: IndexMap<String, (String, String)>,
    pub trans_install: IndexMap<String, String>,
    pub trans_update: IndexMap<String, String>,
    pub variables: Vars,
    tmpl: Templategen,
}

fn lock(l: &ImportedConfigs) -> std::sync::MutexGuard<'_, Vec<String>> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

fn k(s: &str) -> Value {
    Value::String(s.to_string())
}

fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::String(s) => s.is_empty(),
        Value::Sequence(s) => s.is_empty(),
        Value::Mapping(m) => m.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
        _ => false,
    }
}

fn as_map(v: Option<&Value>) -> Mapping {
    match v {
        Some(Value::Mapping(m)) => m.clone(),
        _ => Mapping::new(),
    }
}

/// {**low, **high}
fn merge_vars(high: &Vars, low: &Vars) -> Vars {
    let mut out = low.clone();
    for (key, v) in high {
        out.insert(key.clone(), v.clone());
    }
    out
}

/// {**low, **high} for typed dictionaries
fn merge_maps<T: Clone>(
    high: &IndexMap<String, T>,
    low: &IndexMap<String, T>,
) -> IndexMap<String, T> {
    let mut out = low.clone();
    for (key, v) in high {
        out.insert(key.clone(), v.clone());
    }
    out
}

/// recursive merge as done by the original (including its argument swapping)
fn merge_deep(high: &Mapping, low: &Mapping) -> Result<Mapping> {
    let mut fin = high.clone();
    for (key, val) in low {
        match val {
            Value::Mapping(m) => {
                let cur = as_map(fin.get(key));
                fin.insert(key.clone(), Value::Mapping(merge_deep(m, &cur)?));
            }
            Value::Sequence(s) => {
                let mut cur = match fin.get(key) {
                    Some(Value::Sequence(c)) => c.clone(),
                    _ => Vec::new(),
                };
                cur.extend(s.iter().cloned());
                fin.insert(key.clone(), Value::Sequence(cur));
            }
            Value::String(_) => {
                fin.insert(key.clone(), val.clone());
            }
            _ => return yaml_err("unable to merge"),
        }
    }
    Ok(fin)
}

/// recursively delete all none/empty values in a dictionary
fn clear_none(dic: &Mapping) -> Mapping {
    let mut new = Mapping::new();
    for (key, val) in dic {
        let name = vstr(key);
        if name == KEY_DOTFILE_SRC || name == KEY_DOTFILE_DST {
            new.insert(key.clone(), val.clone());
            continue;
        }
        let newv = match val {
            Value::Mapping(m) => {
                let c = clear_none(m);
                if c.is_empty() {
                    continue;
                }
                Value::Mapping(c)
            }
            Value::Null => continue,
            Value::Sequence(s) if s.is_empty() => continue,
            other => other.clone(),
        };
        new.insert(key.clone(), newv);
    }
    new
}

/// remove nulls everywhere (toml cannot represent them)
fn strip_nulls(v: &Value) -> Value {
    match v {
        Value::Mapping(m) => Value::Mapping(
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(key, v)| (key.clone(), strip_nulls(v)))
                .collect(),
        ),
        Value::Sequence(s) => {
            Value::Sequence(s.iter().filter(|x| !x.is_null()).map(strip_nulls).collect())
        }
        other => other.clone(),
    }
}

/// load a yaml/toml document
fn yaml_load(path: &str) -> Result<(Value, Format)> {
    let content = fs::read_to_string(path)
        .map_err(|e| Error::Yaml(format!("config yaml error: {path}: {e}")))?;
    let err = |e: &dyn std::fmt::Display| {
        let msg = format!("config yaml error: {path}");
        log::err(e);
        Error::Yaml(msg)
    };
    if path.to_lowercase().ends_with(".toml") {
        let table: toml::Table = toml::from_str(&content).map_err(|e| err(&e))?;
        let mut value = serde_yaml_ng::to_value(&table).map_err(|e| err(&e))?;
        if let Value::Mapping(m) = &mut value {
            // toml has no null: handle inexistent dotfiles/profiles
            for key in [KEY_DOTFILES, KEY_PROFILES] {
                if !m.contains_key(key) {
                    m.insert(k(key), Value::Null);
                }
            }
        }
        return Ok((value, Format::Toml));
    }
    let mut value: Value = serde_yaml_ng::from_str(&content).map_err(|e| err(&e))?;
    let _ = value.apply_merge();
    Ok((value, Format::Yaml))
}

fn yaml_dump(content: &Mapping, fmt: Format) -> Result<String> {
    match fmt {
        Format::Yaml => {
            let out = serde_yaml_ng::to_string(&Value::Mapping(content.clone()))
                .map_err(|e| Error::Yaml(e.to_string()))?;
            // keep empty top entries bare like the original output
            let out = out
                .lines()
                .map(|l| match l.strip_suffix(": null") {
                    Some(name) if !name.starts_with(' ') && !name.contains(' ') => {
                        format!("{name}:")
                    }
                    _ => l.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(out + "\n")
        }
        Format::Toml => {
            let clean = strip_nulls(&Value::Mapping(content.clone()));
            toml::to_string(&clean).map_err(|e| Error::Yaml(e.to_string()))
        }
    }
}

impl CfgYaml {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: &str,
        profile: Option<&str>,
        addprofiles: Vec<String>,
        reloading: bool,
        imported_configs: Option<ImportedConfigs>,
        fail_on_error: bool,
    ) -> Result<Self> {
        let path = utils::abspath(path);
        if !utils::exists(&path) {
            let err = format!("invalid config path: \"{path}\"");
            dbg_log!("{err}");
            return yaml_err(err);
        }
        dbg_log!("START of config parsing");
        dbg_log!("reloading: {reloading}");
        dbg_log!("profile: {profile:?}");
        dbg_log!("included profiles: {}", addprofiles.join(","));

        // an empty shared list means "not shared" like in the original
        let imported = match imported_configs {
            Some(l) if !lock(&l).is_empty() => l,
            _ => Arc::new(Mutex::new(Vec::new())),
        };

        let (loaded, format) = yaml_load(&path)?;
        let mut me = Self {
            path,
            profile: profile.map(String::from),
            inc_profiles: addprofiles,
            reloading,
            format,
            dirty: false,
            dirty_deprecated: false,
            profilevarskeys: Vec::new(),
            imported_configs: imported,
            yaml: Mapping::new(),
            settings: Settings::default(),
            dotfiles: IndexMap::new(),
            profiles: IndexMap::new(),
            actions: IndexMap::new(),
            trans_install: IndexMap::new(),
            trans_update: IndexMap::new(),
            variables: Vars::new(),
            tmpl: Templategen::new(".", None, &[], &[]),
        };
        me.yaml = match loaded {
            Value::Mapping(m) => m,
            Value::Null => Mapping::new(),
            _ => return yaml_err("config format error: top level is not a dictionary"),
        };
        let was_empty = me.yaml.is_empty();
        me.fix_deprecated();
        me.validate(was_empty, fail_on_error)?;
        if was_empty {
            for key in [KEY_SETTINGS, KEY_DOTFILES, KEY_PROFILES] {
                me.yaml.insert(k(key), Value::Null);
            }
        }
        me.parse(fail_on_error)?;
        Ok(me)
    }

    fn parse(&mut self, _fail_on_error: bool) -> Result<()> {
        // parse the "config" block
        self.settings = self.parse_blk_settings()?;

        // base templater (when no vars/dvars exist)
        let v = std::mem::take(&mut self.variables);
        self.variables = self.enrich_vars(v);
        self.redefine_templater();

        // variables and dynvariables are merged before being templated
        // in order to allow cyclic references between them
        let mut var = self.parse_blk_variables(KEY_VARIABLES)?;
        self.add_variables(&mut var, false, false, false)?;
        let mut dvariables = self.parse_blk_variables(KEY_DVARIABLES)?;
        self.add_variables(&mut dvariables, false, false, false)?;

        let vars = std::mem::take(&mut self.variables);
        self.variables = self.rec_resolve_variables(vars)?;
        if !dvariables.is_empty() {
            let keys: Vec<String> = dvariables.keys().cloned().collect();
            Self::shell_exec_dvars(&mut self.variables, &keys)?;
        }
        self.redefine_templater();
        self.debug_vars("current variables defined");

        // parse the "profiles" block
        self.profiles = self.parse_blk_profiles()?;

        // include the profile's variables/dynvariables last
        let (incpro, mut pvar, mut pdvar) = self.get_profile_included_vars()?;
        let all: Vec<String> = self
            .inc_profiles
            .iter()
            .chain(incpro.iter())
            .cloned()
            .collect();
        self.inc_profiles = utils::uniq_list(&all);
        self.add_variables(&mut pvar, false, true, true)?;
        self.add_variables(&mut pdvar, true, true, true)?;
        self.profilevarskeys.extend(pvar.keys().cloned());
        self.profilevarskeys.extend(pdvar.keys().cloned());

        // template variables
        let vars = std::mem::take(&mut self.variables);
        self.variables = self.template_dict(&vars)?;
        self.debug_vars("variables defined (after template dict)");

        // template the "include" entries
        self.template_include_entry()?;

        // template config entries
        self.settings.dotpath = self.template_str(&self.settings.dotpath.clone())?;

        // parse the other blocks
        self.dotfiles = self.parse_blk_dotfiles()?;
        self.actions = self.parse_blk_actions(&self.yaml.clone())?;
        self.trans_install = self.parse_blk_trans(KEY_TRANS_INSTALL)?;
        self.trans_update = self.parse_blk_trans(KEY_TRANS_UPDATE)?;

        // import elements
        let mut newvars = self.import_variables()?;
        self.clear_profile_vars(&mut newvars);
        self.add_variables(&mut newvars, false, true, true)?;

        self.import_actions()?;
        self.import_profiles_dotfiles()?;
        self.import_configs()?;

        // process profile include items (actions, dotfiles, ...)
        self.resolve_profile_includes()?;

        // add the current profile variables
        let (_, mut pvar, mut pdvar) = self.get_profile_included_vars()?;
        self.add_variables(&mut pvar, false, true, false)?;
        self.add_variables(&mut pdvar, true, true, false)?;
        self.profilevarskeys.extend(pvar.keys().cloned());
        self.profilevarskeys.extend(pdvar.keys().cloned());

        // resolve variables
        self.clear_profile_vars(&mut newvars);
        self.add_variables(&mut newvars, false, true, false)?;

        // process profile ALL
        self.resolve_profile_all();
        // template dotfiles entries
        self.template_dotfiles_entries()?;

        // parse the "uservariables" block
        let mut uvariables = self.parse_blk_uservariables()?;
        self.add_variables(&mut uvariables, false, false, false)?;

        dbg_log!("END of config parsing");
        Ok(())
    }

    ////////////////////////////////////////////////////////
    // public methods
    ////////////////////////////////////////////////////////

    /// get abs src file from a relative path in dotpath
    pub fn resolve_dotfile_src(
        &self,
        src: &str,
        templater: Option<&Templategen>,
    ) -> Result<String> {
        if src.is_empty() {
            return Ok(String::new());
        }
        let mut new = src.to_string();
        if let Some(t) = templater {
            new = t.generate_string(src)?;
        }
        if new != src {
            dbg_log!("dotfile src: \"{src}\" -> \"{new}\"");
        }
        Ok(self.norm_path(&utils::join(&self.settings.dotpath, &new)))
    }

    pub fn resolve_dotfile_dst(
        &self,
        dst: &str,
        templater: Option<&Templategen>,
    ) -> Result<String> {
        if dst.is_empty() {
            return Ok(String::new());
        }
        let mut new = dst.to_string();
        if let Some(t) = templater {
            new = t.generate_string(dst)?;
        }
        if new != dst {
            dbg_log!("dotfile dst: \"{dst}\" -> \"{new}\"");
        }
        Ok(self.norm_path(&new))
    }

    /// add an existing dotfile key to a profile (created if missing)
    pub fn add_dotfile_to_profile(&mut self, dotfile_key: &str, profile_key: &str) -> Result<bool> {
        self.new_profile(profile_key)?;
        let Some(profile) = self.profiles.get(profile_key) else {
            return Ok(false);
        };
        let pdfs = get_list(profile, KEY_PROFILE_DOTFILES);
        if !pdfs.iter().any(|d| d == KEY_ALL || d == dotfile_key) {
            let profiles = self.yaml_section(KEY_PROFILES);
            let entry = match profiles.get_mut(profile_key) {
                Some(Value::Mapping(m)) => m,
                _ => {
                    profiles.insert(k(profile_key), Value::Mapping(Mapping::new()));
                    match profiles.get_mut(profile_key) {
                        Some(Value::Mapping(m)) => m,
                        _ => return Ok(false),
                    }
                }
            };
            match entry.get_mut(KEY_PROFILE_DOTFILES) {
                Some(Value::Sequence(s)) => s.push(k(dotfile_key)),
                _ => {
                    entry.insert(
                        k(KEY_PROFILE_DOTFILES),
                        Value::Sequence(vec![k(dotfile_key)]),
                    );
                }
            }
            dbg_log!("add \"{dotfile_key}\" to profile \"{profile_key}\"");
            self.dirty = true;
        }
        Ok(self.dirty)
    }

    /// all existing dotfile keys
    pub fn all_dotfile_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.dotfiles.keys().cloned().collect();
        if let Some(Value::Mapping(m)) = self.yaml.get(KEY_DOTFILES) {
            for key in m.keys() {
                let key = vstr(key);
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    pub fn update_dotfile(&mut self, key: &str, chmod: Option<u32>) -> bool {
        if !self.dotfiles.contains_key(key) {
            return false;
        }
        let new = chmod.filter(|c| *c != 0).map(|c| format!("{c:o}"));
        let Some(Value::Mapping(dfs)) = self.yaml.get_mut(KEY_DOTFILES) else {
            return false;
        };
        let Some(Value::Mapping(dotfile)) = dfs.get_mut(key) else {
            return false;
        };
        let old = dotfile.get(KEY_DOTFILE_CHMOD).map(vstr);
        if old.as_deref() == Some(CHMOD_PRESERVE) {
            dbg_log!("ignore chmod change since {CHMOD_PRESERVE}");
            return false;
        }
        if old == new {
            return false;
        }
        dbg_log!("update dotfile: {key} old chmod:{old:?} new chmod:{new:?}");
        match new {
            None => {
                dotfile.remove(KEY_DOTFILE_CHMOD);
            }
            Some(n) => {
                dotfile.insert(k(KEY_DOTFILE_CHMOD), Value::String(n));
            }
        }
        self.dirty = true;
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_dotfile(
        &mut self,
        key: &str,
        src: &str,
        dst: &str,
        link: LinkType,
        chmod: Option<u32>,
        trans_install_key: Option<&str>,
        trans_update_key: Option<&str>,
    ) -> bool {
        if self.dotfiles.contains_key(key) {
            return false;
        }
        dbg_log!("adding new dotfile: {key} src:{src} dst:{dst} link:{link}");
        let mut df = Mapping::new();
        df.insert(k(KEY_DOTFILE_SRC), k(src));
        df.insert(k(KEY_DOTFILE_DST), k(dst));
        if link != self.settings.link_dotfile_default {
            df.insert(k(KEY_DOTFILE_LINK), k(link.as_str()));
        }
        if let Some(c) = chmod {
            df.insert(k(KEY_DOTFILE_CHMOD), k(&format!("{c:o}")));
        }
        if let Some(t) = trans_install_key {
            df.insert(k(KEY_TRANS_INSTALL), k(t));
        }
        if let Some(t) = trans_update_key {
            df.insert(k(KEY_TRANS_UPDATE), k(t));
        }
        self.yaml_section(KEY_DOTFILES)
            .insert(k(key), Value::Mapping(df));
        self.dirty = true;
        true
    }

    pub fn del_dotfile(&mut self, key: &str) -> bool {
        let Some(Value::Mapping(dfs)) = self.yaml.get_mut(KEY_DOTFILES) else {
            log::err(format!("key not in dotfiles: {key}"));
            return false;
        };
        if dfs.remove(key).is_none() {
            log::err(format!("key not in dotfiles: {key}"));
            return false;
        }
        dbg_log!("remove dotfile: {key}");
        self.dirty = true;
        true
    }

    pub fn del_dotfile_from_profile(&mut self, df_key: &str, pro_key: &str) -> bool {
        dbg_log!("removing \"{df_key}\" from \"{pro_key}\"");
        if !self.dotfiles.contains_key(df_key) {
            log::err(format!("key not in dotfiles: {df_key}"));
            return false;
        }
        if !self.profiles.contains_key(pro_key) {
            log::err(format!("key not in profile: {pro_key}"));
            return false;
        }
        let Some(Value::Mapping(pros)) = self.yaml.get_mut(KEY_PROFILES) else {
            return true;
        };
        let Some(Value::Mapping(profile)) = pros.get_mut(pro_key) else {
            return true;
        };
        let Some(Value::Sequence(dfs)) = profile.get_mut(KEY_PROFILE_DOTFILES) else {
            return true;
        };
        let before = dfs.len();
        dfs.retain(|d| vstr(d) != df_key);
        if dfs.len() != before {
            self.dirty = true;
        }
        true
    }

    /// save this instance and return True if saved
    pub fn save(&mut self) -> Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        let mut content = self.prepare_to_save();
        if self.dirty_deprecated {
            let mut settings = as_map(content.get(KEY_SETTINGS));
            settings.insert(k("minversion"), k(VERSION));
            content.insert(k(KEY_SETTINGS), Value::Mapping(settings));
        }
        dbg_log!("saving to {}", self.path);
        let out = yaml_dump(&content, self.format)?;
        fs::write(&self.path, out)
            .map_err(|e| Error::Yaml(format!("error saving config: {}: {e}", self.path)))?;
        if self.dirty_deprecated {
            log::warn("your config contained deprecated entries and was updated");
        }
        self.dirty = false;
        Ok(true)
    }

    pub fn dump(&self) -> Result<String> {
        let content = self.prepare_to_save();
        yaml_dump(&content, self.format)
    }

    fn prepare_to_save(&self) -> Mapping {
        let mut content = clear_none(&self.yaml);
        for key in [KEY_SETTINGS, KEY_DOTFILES, KEY_PROFILES] {
            if !content.contains_key(key) {
                content.insert(k(key), Value::Null);
            }
        }
        content
    }

    /// mutable access to a top level dictionary of the document (created if needed)
    fn yaml_section(&mut self, key: &str) -> &mut Mapping {
        if !matches!(self.yaml.get(key), Some(Value::Mapping(_))) {
            self.yaml.insert(k(key), Value::Mapping(Mapping::new()));
        }
        match self.yaml.get_mut(key) {
            Some(Value::Mapping(m)) => m,
            _ => unreachable!("section was just created"),
        }
    }

    fn new_profile(&mut self, key: &str) -> Result<()> {
        if key == KEY_ALL {
            let err = format!("profile key \"{key}\" is reserved");
            log::warn(&err);
            return yaml_err(err);
        }
        if key.is_empty() {
            log::warn("empty profile key");
            return yaml_err("empty profile key");
        }
        if !self.profiles.contains_key(key) {
            let mut entry = Mapping::new();
            entry.insert(k(KEY_PROFILE_DOTFILES), Value::Sequence(Vec::new()));
            self.yaml_section(KEY_PROFILES)
                .insert(k(key), Value::Mapping(entry.clone()));
            self.profiles.insert(key.to_string(), entry);
            dbg_log!("adding new profile: {key}");
            self.dirty = true;
        }
        Ok(())
    }

    ////////////////////////////////////////////////////////
    // block parsing
    ////////////////////////////////////////////////////////

    /// entry of the document as a dictionary (copy)
    fn entry_map(dic: &Mapping, key: &str, mandatory: bool) -> Result<Mapping> {
        match dic.get(key) {
            None if mandatory => yaml_err(format!("invalid config: no entry \"{key}\" found")),
            Some(Value::Mapping(m)) => Ok(m.clone()),
            Some(v) if is_falsy(v) => Ok(Mapping::new()),
            None => Ok(Mapping::new()),
            Some(_) => yaml_err(format!(
                "config format error: \"{key}\" must be a dictionary"
            )),
        }
    }

    fn parse_blk_settings(&mut self) -> Result<Settings> {
        let block = Self::entry_map(&self.yaml, KEY_SETTINGS, true)?;
        let mut settings = Settings::from_mapping(&block)?;
        if let Some(min) = settings.minversion.clone() {
            Self::check_minversion(&min)?;
        }
        settings.dotpath = self.norm_path(&settings.dotpath);
        settings.workdir = self.norm_path(&settings.workdir);
        settings.filter_file = settings
            .filter_file
            .iter()
            .map(|p| self.norm_path(p))
            .collect();
        settings.func_file = settings
            .func_file
            .iter()
            .map(|p| self.norm_path(p))
            .collect();
        Ok(settings)
    }

    fn parse_blk_dotfiles(&mut self) -> Result<IndexMap<String, Mapping>> {
        let dotfiles = Self::entry_map(&self.yaml, KEY_DOTFILES, true)?;
        let res = self.norm_dotfiles(&dotfiles)?;
        Ok(res)
    }

    fn parse_blk_profiles(&mut self) -> Result<IndexMap<String, Mapping>> {
        let profiles = Self::entry_map(&self.yaml, KEY_PROFILES, true)?;
        self.norm_profiles(&profiles)
    }

    fn parse_blk_actions(&self, dic: &Mapping) -> Result<IndexMap<String, (String, String)>> {
        let actions = Self::entry_map(dic, KEY_ACTIONS, false)?;
        Ok(Self::norm_actions(&actions))
    }

    fn parse_blk_trans(&self, key: &str) -> Result<IndexMap<String, String>> {
        let trans = Self::entry_map(&self.yaml, key, false)?;
        Ok(trans.iter().map(|(a, b)| (vstr(a), vstr(b))).collect())
    }

    fn parse_blk_variables(&self, key: &str) -> Result<Vars> {
        Ok(mapping_to_vars(&Self::entry_map(&self.yaml, key, false)?))
    }

    fn parse_blk_uservariables(&mut self) -> Result<Vars> {
        let uvariables = Self::entry_map(&self.yaml, KEY_UVARIABLES, false)?;
        let mut uvars = Vars::new();
        if !self.reloading {
            for (name, prompt) in &uvariables {
                let name = vstr(name);
                if self.variables.contains_key(&name) {
                    dbg_log!("ignore uservariables {name}");
                    continue;
                }
                let content = utils::userinput(&vstr(prompt));
                uvars.insert(name, Value::String(content));
            }
        }
        if !uvars.is_empty() {
            // saving is best effort
            let _ = self.save_uservariables(&uvars);
        }
        Ok(uvars)
    }

    ////////////////////////////////////////////////////////
    // parsing helpers
    ////////////////////////////////////////////////////////

    fn template_include_entry(&mut self) -> Result<()> {
        self.settings.import_actions = self.template_list(&self.settings.import_actions.clone())?;
        self.settings.import_configs = self.template_list(&self.settings.import_configs.clone())?;
        self.settings.import_variables =
            self.template_list(&self.settings.import_variables.clone())?;
        let keys: Vec<String> = self.profiles.keys().cloned().collect();
        for key in keys {
            let entries = get_list(&self.profiles[&key], KEY_IMPORT_PROFILE_DFS);
            let new = self.template_list(&entries)?;
            if !new.is_empty() {
                let seq = new.iter().map(|s| k(s)).collect();
                if let Some(p) = self.profiles.get_mut(&key) {
                    p.insert(k(KEY_IMPORT_PROFILE_DFS), Value::Sequence(seq));
                }
            }
        }
        Ok(())
    }

    /// ensure each action is either pre or post explicitely
    fn norm_actions(actions: &Mapping) -> IndexMap<String, (String, String)> {
        let mut new = IndexMap::new();
        for (key, val) in actions {
            let key = vstr(key);
            if key == ACTION_PRE || key == ACTION_POST {
                if let Value::Mapping(m) = val {
                    for (a, c) in m {
                        new.insert(vstr(a), (key.clone(), vstr(c)));
                    }
                }
            } else {
                new.insert(key, (ACTION_POST.to_string(), vstr(val)));
            }
        }
        new
    }

    fn check_profile_string_entry(pro: &str, entries: &Mapping, key: &str) -> Result<()> {
        match entries.get(key) {
            None | Some(Value::Null) | Some(Value::String(_)) => Ok(()),
            Some(_) => {
                let err = format!("bad value for \"{key}\" in profile \"{pro}\", must be a string");
                log::err(&err);
                yaml_err(format!("config content error: {err}"))
            }
        }
    }

    fn norm_profiles(&self, profiles: &Mapping) -> Result<IndexMap<String, Mapping>> {
        let mut new = IndexMap::new();
        for (pro, entries) in profiles {
            let pro = vstr(pro);
            if pro == KEY_ALL {
                log::warn(format!(
                    "\"{KEY_ALL}\" is a special profile name, consider renaming to avoid any issue."
                ));
            }
            if pro.is_empty() {
                log::warn("empty profile name");
                continue;
            }
            let Value::Mapping(entries) = entries else {
                // no entries in profile dict
                continue;
            };
            if entries.is_empty() {
                continue;
            }
            Self::check_profile_string_entry(&pro, entries, KEY_PROFILE_DESCRIPTION)?;
            Self::check_profile_string_entry(&pro, entries, KEY_PROFILE_GROUP)?;
            let mut entries = entries.clone();
            if !matches!(entries.get(KEY_PROFILE_DOTFILES), Some(Value::Sequence(_))) {
                entries.insert(k(KEY_PROFILE_DOTFILES), Value::Sequence(Vec::new()));
            }
            new.insert(pro, entries);
        }
        Ok(new)
    }

    fn norm_dotfile_chmod(entry: &mut Mapping) -> Result<()> {
        let value = entry.get(KEY_DOTFILE_CHMOD).map(vstr).unwrap_or_default();
        if value == CHMOD_PRESERVE {
            return Ok(());
        }
        let bad = || {
            let err = format!("bad format for chmod: {value}");
            log::err(&err);
            Error::Yaml(format!("config content error: {err}"))
        };
        if value.len() < 3 || value.parse::<u64>().is_err() {
            return Err(bad());
        }
        if value.chars().any(|c| !('0'..='7').contains(&c)) {
            return Err(bad());
        }
        let mode = u32::from_str_radix(&value, 8).map_err(|_| bad())?;
        entry.insert(k(KEY_DOTFILE_CHMOD), Value::Number(Number::from(mode)));
        Ok(())
    }

    fn norm_dotfiles(&self, dotfiles: &Mapping) -> Result<IndexMap<String, Mapping>> {
        let mut new = IndexMap::new();
        for (key, val) in dotfiles {
            let key = vstr(key);
            let mut val = match val {
                Value::Mapping(m) => m.clone(),
                Value::Null => Mapping::new(),
                _ => return yaml_err(format!("config content error: bad dotfile entry \"{key}\"")),
            };
            if !val.contains_key(KEY_DOTFILE_SRC) {
                val.insert(k(KEY_DOTFILE_SRC), k(&key));
            }
            if !val.contains_key(KEY_DOTFILE_DST) {
                return yaml_err(format!(
                    "config content error: dotfile \"{key}\" has no \"dst\""
                ));
            }
            if !val.contains_key(KEY_DOTFILE_LINK) {
                val.insert(
                    k(KEY_DOTFILE_LINK),
                    k(self.settings.link_dotfile_default.as_str()),
                );
            }
            if !val.contains_key(KEY_DOTFILE_NOEMPTY) {
                val.insert(
                    k(KEY_DOTFILE_NOEMPTY),
                    Value::Bool(self.settings.ignoreempty),
                );
            }
            if !val.contains_key(KEY_DOTFILE_TEMPLATE) {
                val.insert(
                    k(KEY_DOTFILE_TEMPLATE),
                    Value::Bool(self.settings.template_dotfile_default),
                );
            }
            if val.contains_key(KEY_DOTFILE_CHMOD) {
                Self::norm_dotfile_chmod(&mut val)?;
            }
            new.insert(key, val);
        }
        Ok(new)
    }

    /// add new variables
    /// @shell: execute the variable through the shell
    /// @template: template the variable
    /// @prio: new takes priority over existing variables
    fn add_variables(
        &mut self,
        new: &mut Vars,
        shell: bool,
        template: bool,
        prio: bool,
    ) -> Result<()> {
        if new.is_empty() {
            return Ok(());
        }
        if prio {
            self.variables = merge_vars(new, &self.variables);
        } else {
            new.retain(|key, _| !self.variables.contains_key(key));
            self.variables = merge_vars(&self.variables, new);
        }
        let vars = std::mem::take(&mut self.variables);
        self.variables = self.enrich_vars(vars);
        self.redefine_templater();
        if template {
            let vars = std::mem::take(&mut self.variables);
            self.variables = self.rec_resolve_variables(vars)?;
        }
        if shell && !new.is_empty() {
            let keys: Vec<String> = new.keys().cloned().collect();
            Self::shell_exec_dvars(&mut self.variables, &keys)?;
            self.redefine_templater();
        }
        Ok(())
    }

    fn enrich_vars(&self, mut variables: Vars) -> Vars {
        if let Some(p) = &self.profile {
            variables.insert("profile".into(), k(p));
        }
        variables.insert(
            "_dotdrop_dotpath".into(),
            k(&self.norm_path(&self.settings.dotpath)),
        );
        variables.insert("_dotdrop_cfgpath".into(), k(&self.norm_path(&self.path)));
        variables.insert(
            "_dotdrop_workdir".into(),
            k(&self.norm_path(&self.settings.workdir)),
        );
        variables
    }

    /// recursively get included <keyitem> in profile
    fn get_profile_included_item(&self, keyitem: &str) -> Result<Vars> {
        let mut profiles: Vec<Option<String>> = vec![self.profile.clone()];
        profiles.extend(self.inc_profiles.iter().cloned().map(Some));
        let mut items = Vars::new();
        for profile in profiles {
            let seen: Vec<String> = profile.iter().cloned().collect();
            let i = self.included_item(profile.as_deref(), keyitem, &seen)?;
            items = merge_vars(&i, &items);
        }
        Ok(items)
    }

    fn included_item(&self, profile: Option<&str>, keyitem: &str, seen: &[String]) -> Result<Vars> {
        let mut items = Vars::new();
        let Some(profile) = profile else {
            return Ok(items);
        };
        let Some(pentry) = self.profiles.get(profile) else {
            return Ok(items);
        };
        for inherited in get_list(pentry, KEY_PROFILE_INCLUDE) {
            if inherited == profile || seen.contains(&inherited) {
                return yaml_err("\"include\" loop");
            }
            let mut seen2 = seen.to_vec();
            seen2.push(inherited.clone());
            let new = self.included_item(Some(&inherited), keyitem, &seen2)?;
            dbg_log!("included {keyitem} from {inherited}: {new:?}");
            for (key, v) in new {
                items.insert(key, v);
            }
        }
        let cur = mapping_to_vars(&as_map(pentry.get(keyitem)));
        Ok(merge_vars(&cur, &items))
    }

    /// profile -> ALL
    fn resolve_profile_all(&mut self) {
        let all_keys: Vec<Value> = self.dotfiles.keys().map(|d| k(d)).collect();
        for (key, val) in self.profiles.iter_mut() {
            let dfs = get_list(val, KEY_PROFILE_DOTFILES);
            if dfs.iter().any(|d| d == KEY_ALL) {
                dbg_log!("add ALL to profile \"{key}\"");
                val.insert(k(KEY_PROFILE_DOTFILES), Value::Sequence(all_keys.clone()));
            }
        }
    }

    fn resolve_profile_includes(&mut self) -> Result<()> {
        let keys: Vec<String> = self.profiles.keys().cloned().collect();
        for key in keys {
            self.rec_resolve_profile_include(&key, &mut Vec::new())?;
        }
        Ok(())
    }

    /// recursively resolve include of other profiles's dotfiles and actions
    fn rec_resolve_profile_include(
        &mut self,
        profile: &str,
        visiting: &mut Vec<String>,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let Some(this) = self.profiles.get(profile) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut dotfiles = get_list(this, KEY_PROFILE_DOTFILES);
        let mut actions = get_list(this, KEY_PROFILE_ACTIONS);
        let includes = get_list(this, KEY_PROFILE_INCLUDE);
        if includes.is_empty() {
            return Ok((dotfiles, actions));
        }
        if visiting.iter().any(|v| v == profile) {
            return yaml_err("\"include loop\"");
        }
        visiting.push(profile.to_string());
        dbg_log!("{profile} includes {}", includes.join(","));

        for i in utils::uniq_list(&includes) {
            dbg_log!("resolving includes \"{profile}\" <- \"{i}\"");
            if !self.profiles.contains_key(&i) {
                log::warn(format!("include unknown profile: {i}"));
                continue;
            }
            let (o_dfs, o_actions) = self.rec_resolve_profile_include(&i, visiting)?;
            dotfiles.extend(o_dfs);
            let uniq_dfs: Vec<Value> = utils::uniq_list(&dotfiles).iter().map(|s| k(s)).collect();
            actions.extend(o_actions);
            let uniq_acts: Vec<Value> = utils::uniq_list(&actions).iter().map(|s| k(s)).collect();
            if let Some(p) = self.profiles.get_mut(profile) {
                p.insert(k(KEY_PROFILE_DOTFILES), Value::Sequence(uniq_dfs));
                p.insert(k(KEY_PROFILE_ACTIONS), Value::Sequence(uniq_acts));
            }
        }
        visiting.pop();

        let this = &self.profiles[profile];
        let dotfiles = get_list(this, KEY_PROFILE_DOTFILES);
        let actions = get_list(this, KEY_PROFILE_ACTIONS);
        if let Some(p) = self.profiles.get_mut(profile) {
            p.insert(k(KEY_PROFILE_INCLUDE), Value::Sequence(Vec::new()));
        }
        Ok((dotfiles, actions))
    }

    ////////////////////////////////////////////////////////
    // imported entries
    ////////////////////////////////////////////////////////

    fn import_variables(&mut self) -> Result<Vars> {
        let paths = self.settings.import_variables.clone();
        if paths.is_empty() {
            return Ok(Vars::new());
        }
        let paths = self.resolve_paths(&paths)?;
        let mut newvars = Vars::new();
        for path in paths {
            dbg_log!("import variables from {path}");
            let var = mapping_to_vars(&self.import_sub_map(&path, KEY_VARIABLES)?);
            let dvar = mapping_to_vars(&self.import_sub_map(&path, KEY_DVARIABLES)?);
            let merged = merge_vars(&dvar, &var);
            let mut merged = self.rec_resolve_variables(merged)?;
            if !dvar.is_empty() {
                let keys: Vec<String> = dvar.keys().cloned().collect();
                Self::shell_exec_dvars(&mut merged, &keys)?;
            }
            self.clear_profile_vars(&mut merged);
            newvars = merge_vars(&merged, &newvars);
        }
        Ok(newvars)
    }

    fn import_actions(&mut self) -> Result<()> {
        let paths = self.settings.import_actions.clone();
        if paths.is_empty() {
            return Ok(());
        }
        for path in self.resolve_paths(&paths)? {
            dbg_log!("import actions from {path}");
            let ext = Self::load_other(&path)?;
            let new = self.parse_blk_actions(&ext)?;
            let mut merged = self.actions.clone();
            for (key, v) in new {
                merged.insert(key, v);
            }
            self.actions = merged;
        }
        Ok(())
    }

    fn import_profiles_dotfiles(&mut self) -> Result<()> {
        let keys: Vec<String> = self.profiles.keys().cloned().collect();
        for key in keys {
            let imp = get_list(&self.profiles[&key], KEY_IMPORT_PROFILE_DFS);
            if imp.is_empty() {
                continue;
            }
            dbg_log!("import dotfiles for profile {key}");
            for path in self.resolve_paths(&imp)? {
                let ext = Self::load_other(&path)?;
                let new = match ext.get(KEY_DOTFILES) {
                    Some(Value::Sequence(s)) => s.clone(),
                    _ => Vec::new(),
                };
                let current = get_list(&self.profiles[&key], KEY_DOTFILES);
                let mut all = new;
                all.extend(current.iter().map(|c| k(c)));
                if let Some(p) = self.profiles.get_mut(&key) {
                    p.insert(k(KEY_DOTFILES), Value::Sequence(all));
                }
            }
        }
        Ok(())
    }

    fn import_config(&mut self, path: &str) -> Result<()> {
        dbg_log!("import config from {path}");
        let sub = CfgYaml::new(
            path,
            self.profile.as_deref(),
            self.inc_profiles.clone(),
            false,
            Some(self.imported_configs.clone()),
            false,
        )?;

        // settings are ignored from external file except for filter_file and func_file
        let funcs: Vec<String> = sub
            .settings
            .func_file
            .iter()
            .map(|f| self.norm_path(f))
            .collect();
        let filters: Vec<String> = sub
            .settings
            .filter_file
            .iter()
            .map(|f| self.norm_path(f))
            .collect();
        self.settings.func_file.extend(funcs);
        self.settings.filter_file.extend(filters);

        // merge top entries
        self.dotfiles = merge_maps(&self.dotfiles, &sub.dotfiles);
        let main: Mapping = self
            .profiles
            .iter()
            .map(|(a, b)| (k(a), Value::Mapping(b.clone())))
            .collect();
        let other: Mapping = sub
            .profiles
            .iter()
            .map(|(a, b)| (k(a), Value::Mapping(b.clone())))
            .collect();
        let merged = merge_deep(&main, &other)?;
        self.profiles = merged
            .iter()
            .map(|(a, b)| (vstr(a), as_map(Some(b))))
            .collect();
        self.actions = merge_maps(&self.actions, &sub.actions);
        self.trans_install = merge_maps(&self.trans_install, &sub.trans_install);
        self.trans_update = merge_maps(&self.trans_update, &sub.trans_update);
        let mut subvars = sub.variables.clone();
        self.clear_profile_vars(&mut subvars);

        {
            let sub_list = lock(&sub.imported_configs).clone();
            let mut list = lock(&self.imported_configs);
            list.push(path.to_string());
            list.extend(sub_list);
        }
        self.add_variables(&mut subvars, false, true, true)
    }

    fn import_configs(&mut self) -> Result<()> {
        let imp = self.settings.import_configs.clone();
        if imp.is_empty() {
            return Ok(());
        }
        for path in self.resolve_paths(&imp)? {
            if lock(&self.imported_configs).contains(&path) {
                return yaml_err(format!("{path} imported more than once in {}", self.path));
            }
            self.import_config(&path)?;
        }
        Ok(())
    }

    /// load the dictionary of another config file
    fn load_other(path: &str) -> Result<Mapping> {
        match yaml_load(path)?.0 {
            Value::Mapping(m) => Ok(m),
            _ => Ok(Mapping::new()),
        }
    }

    fn import_sub_map(&self, path: &str, key: &str) -> Result<Mapping> {
        dbg_log!("import \"{key}\" from \"{path}\"");
        Self::entry_map(&Self::load_other(path)?, key, false)
    }

    ////////////////////////////////////////////////////////
    // deprecated entries
    ////////////////////////////////////////////////////////

    fn fix_deprecated(&mut self) {
        if self.yaml.is_empty() {
            return;
        }
        self.fix_deprecated_link_by_default();
        self.fix_deprecated_dotfile_link();
        self.fix_deprecated_trans();
    }

    fn mark_deprecated(&mut self) {
        self.dirty = true;
        self.dirty_deprecated = true;
    }

    fn fix_deprecated_trans_in_dict(dic: &mut Mapping) -> Vec<String> {
        let mut warns = Vec::new();
        for (old, new) in [
            (OLD_KEY_TRANS, KEY_TRANS_INSTALL),
            (OLD_KEY_TRANS_R, KEY_TRANS_INSTALL),
            (OLD_KEY_TRANS_W, KEY_TRANS_UPDATE),
        ] {
            if let Some(v) = dic.remove(old) {
                dic.insert(k(new), v);
                warns.push(format!("deprecated \"{old}\", updated to \"{new}\""));
            }
        }
        warns
    }

    fn fix_deprecated_trans(&mut self) {
        let mut warns = Self::fix_deprecated_trans_in_dict(&mut self.yaml);
        if let Some(Value::Mapping(dfs)) = self.yaml.get_mut(KEY_DOTFILES) {
            for (_, val) in dfs.iter_mut() {
                if let Value::Mapping(m) = val {
                    warns.extend(Self::fix_deprecated_trans_in_dict(m));
                }
            }
        }
        if !warns.is_empty() {
            self.mark_deprecated();
        }
        for w in warns {
            log::warn(w);
        }
    }

    fn fix_deprecated_link_by_default(&mut self) {
        let Some(Value::Mapping(config)) = self.yaml.get_mut(KEY_SETTINGS) else {
            return;
        };
        let Some(old) = config.remove("link_by_default") else {
            return;
        };
        let new = if is_falsy(&old) {
            LinkType::NoLink
        } else {
            LinkType::Link
        };
        config.insert(k("link_on_import"), k(new.as_str()));
        log::warn("deprecated \"link_by_default\"");
        self.mark_deprecated();
    }

    fn fix_deprecated_dotfile_link(&mut self) {
        let mut warns = Vec::new();
        if let Some(Value::Mapping(dfs)) = self.yaml.get_mut(KEY_DOTFILES) {
            for (_, dotfile) in dfs.iter_mut() {
                let Value::Mapping(dotfile) = dotfile else {
                    continue;
                };
                if let Some(Value::Bool(cur)) = dotfile.get(KEY_DOTFILE_LINK).cloned() {
                    let new = if cur { "link" } else { "nolink" };
                    dotfile.insert(k(KEY_DOTFILE_LINK), k(new));
                    warns.push(format!(
                        "deprecated \"link: <boolean>\", updated to \"link: {new}\""
                    ));
                }
                if dotfile.get(KEY_DOTFILE_LINK).map(vstr).as_deref() == Some("link") {
                    dotfile.insert(k(KEY_DOTFILE_LINK), k("absolute"));
                    warns.push(
                        "deprecated \"link: link\", updated to \"link: absolute\"".to_string(),
                    );
                }
                if let Some(Value::Bool(cur)) = dotfile.get("link_children").cloned() {
                    let new = if cur { "link_children" } else { "nolink" };
                    dotfile.remove("link_children");
                    dotfile.insert(k(KEY_DOTFILE_LINK), k(new));
                    warns.push(format!(
                        "deprecated \"link_children\" value, updated to \"{new}\""
                    ));
                }
            }
        }
        if !warns.is_empty() {
            self.mark_deprecated();
        }
        for w in warns {
            log::warn(w);
        }
    }

    ////////////////////////////////////////////////////////
    // validation
    ////////////////////////////////////////////////////////

    fn validate(&self, empty: bool, fail_on_error: bool) -> Result<()> {
        if empty {
            if fail_on_error {
                return yaml_err("empty config file");
            }
            return Ok(());
        }
        for entry in [KEY_DOTFILES, KEY_SETTINGS, KEY_PROFILES] {
            if !self.yaml.contains_key(entry) {
                let err = format!("no {entry} entry found");
                log::err(&err);
                return yaml_err(format!("config format error: {err}"));
            }
        }
        match self.yaml.get(KEY_SETTINGS) {
            Some(Value::Mapping(settings)) if !settings.is_empty() => {
                if let Some(v) = settings.get("link_dotfile_default") {
                    let val = vstr(v);
                    if LinkType::parse(&val).is_err() {
                        let err = format!("bad link value: {val}");
                        log::err(&err);
                        log::err("allowed: [nolink, link, link_children, absolute, relative]");
                        return yaml_err(format!("config content error: {err}"));
                    }
                }
                Ok(())
            }
            _ if fail_on_error => yaml_err(format!("empty \"{KEY_SETTINGS}\" key")),
            _ => Ok(()),
        }
    }

    ////////////////////////////////////////////////////////
    // templating
    ////////////////////////////////////////////////////////

    fn redefine_templater(&mut self) {
        self.tmpl = Templategen::new(
            ".",
            Some(&self.variables),
            &self.settings.func_file,
            &self.settings.filter_file,
        );
    }

    /// template a string until no template marker is left
    fn template_str(&self, item: &str) -> Result<String> {
        let mut val = item.to_string();
        let mut passes = 0;
        while Templategen::string_is_template(&val) {
            passes += 1;
            if passes > MAX_TEMPLATE_PASSES {
                return Err(Error::Undefined(format!("recursive template: {item}")));
            }
            val = self.tmpl.generate_string(&val)?;
        }
        Ok(val)
    }

    fn template_item(&self, item: &Value) -> Result<Value> {
        match item {
            Value::String(s) => Ok(Value::String(self.template_str(s)?)),
            Value::Sequence(s) => Ok(Value::Sequence(
                s.iter()
                    .map(|i| self.template_item(i))
                    .collect::<Result<_>>()?,
            )),
            Value::Mapping(m) => {
                let mut new = Mapping::new();
                for (key, v) in m {
                    new.insert(key.clone(), self.template_item(v)?);
                }
                Ok(Value::Mapping(new))
            }
            other => Ok(other.clone()),
        }
    }

    fn template_list(&self, entries: &[String]) -> Result<Vec<String>> {
        entries
            .iter()
            .map(|e| {
                let new = self.template_str(e)?;
                if *e != new {
                    dbg_log!("resolved: {e} -> {new}");
                }
                Ok(new)
            })
            .collect()
    }

    fn template_dict(&self, entries: &Vars) -> Result<Vars> {
        entries
            .iter()
            .map(|(key, v)| Ok((key.clone(), self.template_item(v)?)))
            .collect()
    }

    fn resolve_dotfile_link(&self, link: &str) -> Result<String> {
        let newlink = self.template_str(link)?;
        if LinkType::parse(&newlink).is_err() {
            let err = format!("bad link value: {newlink}");
            log::err(&err);
            log::err("allowed: [nolink, link, link_children, absolute, relative]");
            return yaml_err(format!("config content error: {err}"));
        }
        Ok(newlink)
    }

    fn template_dotfiles_entries(&mut self) -> Result<()> {
        dbg_log!("templating dotfiles entries");
        // make sure no dotfiles path is None
        for dotfile in self.dotfiles.values_mut() {
            for key in [KEY_DOTFILE_SRC, KEY_DOTFILE_DST] {
                if matches!(dotfile.get(key), Some(Value::Null) | None) {
                    dotfile.insert(k(key), k(""));
                }
            }
        }
        // resolve links before taking subset of dotfiles
        let keys: Vec<String> = self.dotfiles.keys().cloned().collect();
        for key in &keys {
            if let Some(link) = get_str(&self.dotfiles[key], KEY_DOTFILE_LINK) {
                let newlink = self.resolve_dotfile_link(&link)?;
                if let Some(d) = self.dotfiles.get_mut(key) {
                    d.insert(k(KEY_DOTFILE_LINK), k(&newlink));
                }
            }
        }

        // only keep dotfiles related to the selected profile
        let mut pdfs: Vec<String> = Vec::new();
        if let Some(p) = self.profile.as_ref().and_then(|p| self.profiles.get(p)) {
            pdfs = get_list(p, KEY_PROFILE_DOTFILES);
        }
        for addpro in &self.inc_profiles {
            if let Some(p) = self.profiles.get(addpro) {
                pdfs.extend(get_list(p, KEY_PROFILE_DOTFILES));
                pdfs = utils::uniq_list(&pdfs);
            }
        }
        let selected: Vec<String> = if pdfs.iter().any(|d| d == KEY_ALL) {
            keys.clone()
        } else {
            keys.iter()
                .filter(|key| pdfs.contains(key))
                .cloned()
                .collect()
        };

        for key in selected {
            let (src, dst) = {
                let d = &self.dotfiles[&key];
                (
                    get_str(d, KEY_DOTFILE_SRC).unwrap_or_default(),
                    get_str(d, KEY_DOTFILE_DST).unwrap_or_default(),
                )
            };
            let newsrc = self.resolve_dotfile_src(&src, Some(&self.tmpl))?;
            let newdst = self.resolve_dotfile_dst(&dst, Some(&self.tmpl))?;
            if let Some(d) = self.dotfiles.get_mut(&key) {
                d.insert(k(KEY_DOTFILE_SRC), k(&newsrc));
                d.insert(k(KEY_DOTFILE_DST), k(&newdst));
            }
        }
        Ok(())
    }

    /// recursively resolve variables
    fn rec_resolve_variables(&self, variables: Vars) -> Result<Vars> {
        let var = self.enrich_vars(variables);
        let mut templ = Templategen::new(
            ".",
            Some(&var),
            &self.settings.func_file,
            &self.settings.filter_file,
        );
        let mut newvars = var.clone();
        for (key, orig) in &var {
            let mut val = orig.clone();
            let mut passes = 0;
            while Templategen::var_is_template(&val) {
                passes += 1;
                if passes > MAX_TEMPLATE_PASSES {
                    return Err(Error::Undefined(format!("recursive variable: {key}")));
                }
                val = templ.generate_string_or_dict(&val)?;
                if let Value::Mapping(m) = &val {
                    for (sub, subval) in m {
                        newvars.insert(format!("{key}.{}", vstr(sub)), subval.clone());
                    }
                }
                newvars.insert(key.clone(), val.clone());
                templ.update_variables(&newvars);
            }
        }
        Ok(newvars)
    }

    /// resolve profile included variables/dynvariables
    /// returns inc_profiles, inc_var, inc_dvar
    fn get_profile_included_vars(&mut self) -> Result<(Vec<String>, Vars, Vars)> {
        let keys: Vec<String> = self.profiles.keys().cloned().collect();
        for key in keys {
            let incs = get_list(&self.profiles[&key], KEY_PROFILE_INCLUDE);
            if incs.is_empty() {
                continue;
            }
            let new: Vec<Value> = incs
                .iter()
                .map(|e| self.tmpl.generate_string(e).map(|s| k(&s)))
                .collect::<Result<_>>()?;
            if let Some(p) = self.profiles.get_mut(&key) {
                p.insert(k(KEY_PROFILE_INCLUDE), Value::Sequence(new));
            }
        }
        let pro_var = self.get_profile_included_item(KEY_PROFILE_VARIABLES)?;
        let pro_dvar = self.get_profile_included_item(KEY_PROFILE_DVARIABLES)?;
        let inc_profiles = self
            .profile
            .as_ref()
            .and_then(|p| self.profiles.get(p))
            .map(|p| get_list(p, KEY_PROFILE_INCLUDE))
            .unwrap_or_default();
        Ok((inc_profiles, pro_var, pro_dvar))
    }

    ////////////////////////////////////////////////////////
    // helpers
    ////////////////////////////////////////////////////////

    /// remove profile variables from dic to avoid them being overwritten
    fn clear_profile_vars(&self, dic: &mut Vars) {
        for key in &self.profilevarskeys {
            dic.shift_remove(key);
        }
    }

    /// parse an import path in a tuple (path, fatal_not_found)
    fn parse_extended_import_path(path_entry: &str) -> (String, bool) {
        let (path, attribute) = match path_entry.rfind(IMPORT_SEP) {
            Some(i) => (&path_entry[..i], &path_entry[i + 1..]),
            None => ("", path_entry),
        };
        let fatal = attribute != IMPORT_IGNORE_KEY;
        if attribute.is_empty() || attribute == IMPORT_IGNORE_KEY {
            (path.to_string(), fatal)
        } else {
            (path_entry.to_string(), true)
        }
    }

    fn handle_non_existing_path(path: &str, fatal: bool) -> Result<()> {
        let error = format!("bad path {path}");
        if fatal {
            return yaml_err(error);
        }
        log::warn(error);
        Ok(())
    }

    /// normalize, expand globs and check existence of an import entry
    fn process_path(&self, path_entry: &str) -> Result<Vec<String>> {
        let (path, fatal) = Self::parse_extended_import_path(path_entry);
        let path = self.norm_path(&path);
        let paths: Vec<String> = if path.contains('*') || path.contains('?') {
            let opts = glob::MatchOptions {
                require_literal_leading_dot: true,
                ..Default::default()
            };
            glob::glob_with(&utils::expanduser(&path), opts)
                .map(|g| {
                    g.filter_map(|p| p.ok())
                        .map(|p| utils::os(p.as_os_str()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![path.clone()]
        };
        if paths.is_empty() {
            Self::handle_non_existing_path(&path, fatal)?;
            return Ok(Vec::new());
        }
        let mut res = Vec::new();
        for p in paths {
            if utils::exists(&p) {
                res.push(p);
            } else {
                Self::handle_non_existing_path(&p, fatal)?;
            }
        }
        Ok(res)
    }

    fn resolve_paths(&self, paths: &[String]) -> Result<Vec<String>> {
        let mut res = Vec::new();
        for p in paths {
            res.extend(self.process_path(p)?);
        }
        Ok(res)
    }

    /// resolve a path either absolute or relative to config path
    pub fn norm_path(&self, path: &str) -> String {
        if path.is_empty() {
            return String::new();
        }
        let mut path = utils::expanduser(path);
        if !utils::is_abs(&path) {
            path = utils::join(&utils::dirname(&self.path), &path);
        }
        utils::normpath(&path)
    }

    /// shell execute dynvariables in-place
    fn shell_exec_dvars(dic: &mut Vars, keys: &[String]) -> Result<()> {
        for key in keys {
            let Some(val) = dic.get(key) else {
                return yaml_err(format!("dynvariable \"{key}\" not found"));
            };
            let cmd = vstr(val);
            let (ok, out) = utils::shellrun(&cmd);
            if !ok {
                let err = format!("var \"{key}: {cmd}\" failed: {out}");
                log::err(&err);
                return yaml_err(err);
            }
            dbg_log!("{key}: `{cmd}` -> {out}");
            dic.insert(key.clone(), Value::String(out));
        }
        Ok(())
    }

    fn check_minversion(minversion: &str) -> Result<()> {
        if minversion.is_empty() {
            return Ok(());
        }
        let (Some(cur), Some(cfg)) = (
            utils::parse_version(VERSION),
            utils::parse_version(minversion),
        ) else {
            return yaml_err(format!("bad version: \"{VERSION}\" VS \"{minversion}\""));
        };
        if cur < cfg {
            return yaml_err(
                "current dotdrop version is too old for that config file. Please update.",
            );
        }
        Ok(())
    }

    fn debug_vars(&self, title: &str) {
        dbg_log!("{title}:");
        for (key, v) in &self.variables {
            dbg_log!("  - \"{key}\": {}", vstr(v));
        }
    }

    /// save uservariables to a file next to the config
    fn save_uservariables(&self, uvars: &Vars) -> Result<()> {
        let parent = utils::dirname(&self.path);
        let mut cnt = 0;
        let path = loop {
            let name = if cnt == 0 {
                "uservariables.yaml".to_string()
            } else {
                format!("uservariables-{cnt}.yaml")
            };
            cnt += 1;
            let p = utils::join(&parent, &name);
            if !utils::lexists(&p) {
                break p;
            }
        };
        let mut content = Mapping::new();
        let vars: Mapping = uvars.iter().map(|(a, b)| (k(a), b.clone())).collect();
        content.insert(k("variables"), Value::Mapping(vars));
        let out = yaml_dump(&content, self.format)?;
        if fs::write(&path, out).is_err() {
            let err = format!("error saving uservariables to {path}");
            log::err(&err);
            return yaml_err(err);
        }
        log::log(&format!("uservariables values saved to {path}"));
        Ok(())
    }
}
