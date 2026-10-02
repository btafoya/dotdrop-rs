//! Jinja2-style templating with dotdrop's `{{@@ ... @@}}` delimiters.

use crate::dbg_log;
use crate::error::{Error, Result};
use crate::log;
use crate::utils;
use indexmap::IndexMap;
use minijinja::syntax::SyntaxConfig;
use minijinja::value::ValueKind;
use minijinja::{AutoEscape, Environment, ErrorKind, UndefinedBehavior};
use serde_yaml_ng::Value;
use std::collections::HashSet;
use std::fs;
use std::sync::{Mutex, OnceLock};

pub const BLOCK_START: &str = "{%@@";
pub const BLOCK_END: &str = "@@%}";
pub const VAR_START: &str = "{{@@";
pub const VAR_END: &str = "@@}}";
pub const COMMENT_START: &str = "{#@@";
pub const COMMENT_END: &str = "@@#}";

const DICT_ENV_NAME: &str = "env";
const DICT_VARS_NAME: &str = "_vars";
const ENV_DOTDROP_MIME_TEXT: &str = "DOTDROP_MIME_TEXT";

/// ordered dictionary of template variables
pub type Vars = IndexMap<String, Value>;

pub struct Templategen {
    base: String,
    variables: Vars,
    initial_vars: Vars,
    env: Environment<'static>,
    mime_text: Vec<String>,
}

/// warn once per file that python helper modules cannot be loaded
fn warn_once(path: &str, what: &str) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let set = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    if set
        .lock()
        .map(|mut s| s.insert(path.to_string()))
        .unwrap_or(false)
    {
        log::warn(format!(
            "{what} \"{path}\" ignored: python modules are not supported by this implementation"
        ));
    }
}

/// python-like `repr` of a template value
fn py_repr(v: &minijinja::Value) -> String {
    match v.kind() {
        ValueKind::Undefined | ValueKind::None => "None".to_string(),
        ValueKind::Bool => if v.is_true() { "True" } else { "False" }.to_string(),
        ValueKind::String => {
            let s = v.as_str().unwrap_or_default();
            if s.contains('\'') && !s.contains('"') {
                format!("\"{s}\"")
            } else {
                format!(
                    "'{}'",
                    s.replace('\\', "\\\\")
                        .replace('\'', "\\'")
                        .replace('\n', "\\n")
                )
            }
        }
        ValueKind::Seq | ValueKind::Iterable => {
            let items: Vec<String> = v
                .try_iter()
                .map(|it| it.map(|i| py_repr(&i)).collect())
                .unwrap_or_default();
            format!("[{}]", items.join(", "))
        }
        ValueKind::Map => {
            let mut items = Vec::new();
            if let Ok(keys) = v.try_iter() {
                for k in keys {
                    let val = v.get_item(&k).unwrap_or(minijinja::Value::UNDEFINED);
                    items.push(format!("{}: {}", py_repr(&k), py_repr(&val)));
                }
            }
            format!("{{{}}}", items.join(", "))
        }
        _ => v.to_string(),
    }
}

fn to_error(exc: minijinja::Error) -> Error {
    match exc.kind() {
        ErrorKind::UndefinedError => Error::Undefined(format!("undefined variable: {exc}")),
        _ => {
            let mut msg = format!("template error: {exc}");
            let mut src = std::error::Error::source(&exc);
            while let Some(s) = src {
                msg.push_str(&format!(": {s}"));
                src = s.source();
            }
            Error::Undefined(msg)
        }
    }
}

fn build_env(base: &str) -> Environment<'static> {
    let mut env = Environment::new();
    let syntax = SyntaxConfig::builder()
        .block_delimiters(BLOCK_START, BLOCK_END)
        .variable_delimiters(VAR_START, VAR_END)
        .comment_delimiters(COMMENT_START, COMMENT_END)
        .build()
        .expect("static syntax config is valid");
    env.set_syntax(syntax);
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    env.set_keep_trailing_newline(true);
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    env.set_formatter(|out, state, value| match value.kind() {
        ValueKind::Bool | ValueKind::None | ValueKind::Seq | ValueKind::Map => {
            write!(out, "{}", py_repr(value))
                .map_err(|_| minijinja::Error::new(ErrorKind::WriteFailure, "formatting failed"))
        }
        _ => minijinja::escape_formatter(out, state, value),
    });

    // templates are looked up relatively to the dotpath (also outside of it)
    let base = base.to_string();
    env.set_loader(move |name| {
        let path = utils::normpath(&utils::join(&base, name));
        match fs::read(&path) {
            Ok(content) => Ok(Some(String::from_utf8_lossy(&content).into_owned())),
            Err(_) => Ok(None),
        }
    });

    env.add_function("header", |prepend: Option<String>| {
        format!("{}{}", prepend.unwrap_or_default(), utils::header())
    });
    env.add_function("exists", |path: String| {
        utils::exists(&utils::expandvars(&path))
    });
    env.add_function("exists_in_path", |name: String, path: Option<String>| {
        utils::which(&name, path.as_deref()).is_some()
    });
    env.add_function("basename", |path: String| utils::basename(&path));
    env.add_function("dirname", |path: String| utils::dirname(&path));
    env
}

impl Templategen {
    /// @base: directory path where to search for templates
    /// @variables: dictionary of variables for templates
    /// @func_file/@filter_file: python modules, not supported (warned)
    pub fn new(
        base: &str,
        variables: Option<&Vars>,
        func_file: &[String],
        filter_file: &[String],
    ) -> Self {
        dbg_log!("loading templategen");
        let base = base.trim_end_matches('/').to_string();
        let mime_text = std::env::var(ENV_DOTDROP_MIME_TEXT)
            .map(|m| m.split(',').map(|x| x.trim().to_lowercase()).collect())
            .unwrap_or_default();
        for f in func_file {
            warn_once(f, "func_file");
        }
        for f in filter_file {
            warn_once(f, "filter_file");
        }
        let mut vars = Vars::new();
        let mut initial = Vars::new();
        if let Some(v) = variables {
            vars.extend(v.iter().map(|(k, v)| (k.clone(), v.clone())));
            initial = v.clone();
        }
        Self {
            env: build_env(&base),
            base,
            variables: vars,
            initial_vars: initial,
            mime_text,
        }
    }

    /// build the rendering context
    fn context(&self) -> minijinja::Value {
        let environ: IndexMap<String, String> = std::env::vars().collect();
        let mut ctx: IndexMap<String, minijinja::Value> = IndexMap::new();
        ctx.insert(
            DICT_ENV_NAME.into(),
            minijinja::Value::from_serialize(&environ),
        );
        for (k, v) in &self.variables {
            ctx.insert(k.clone(), minijinja::Value::from_serialize(v));
        }
        if !self.initial_vars.is_empty() {
            ctx.insert(
                DICT_VARS_NAME.into(),
                minijinja::Value::from_serialize(&self.initial_vars),
            );
        }
        ctx.into_iter().collect()
    }

    /// render a template file, returns the new content
    pub fn generate(&self, src: &str) -> Result<Vec<u8>> {
        if !utils::exists(src) {
            return Ok(Vec::new());
        }
        let filetype = self.get_filetype(src);
        let istext = self.is_text(&filetype);
        dbg_log!("filetype \"{src}\": {filetype}");
        dbg_log!("is text \"{src}\": {istext}");
        if !istext {
            return self.handle_bin_file(src);
        }
        self.handle_text_file(src)
    }

    pub fn generate_string(&self, string: &str) -> Result<String> {
        if string.is_empty() {
            return Ok(String::new());
        }
        self.env
            .render_str(string, self.context())
            .map_err(to_error)
    }

    /// template (recursively) the string values of a dict
    pub fn generate_dict(&self, dic: &Value) -> Result<Value> {
        match dic {
            Value::Mapping(m) => {
                let mut new = serde_yaml_ng::Mapping::new();
                for (k, v) in m {
                    new.insert(k.clone(), self.generate_dict(v)?);
                }
                Ok(Value::Mapping(new))
            }
            Value::String(s) => Ok(Value::String(self.generate_string(s)?)),
            other => Ok(other.clone()),
        }
    }

    pub fn generate_string_or_dict(&self, content: &Value) -> Result<Value> {
        match content {
            Value::String(s) => Ok(Value::String(self.generate_string(s)?)),
            Value::Mapping(_) => self.generate_dict(content),
            other => Err(Error::Undefined(format!("could not template {other:?}"))),
        }
    }

    /// add vars to the globals, returns the saved variables for `restore_vars`
    pub fn add_tmp_vars(&mut self, newvars: Option<&Vars>) -> Vars {
        let saved = self.variables.clone();
        if let Some(n) = newvars {
            for (k, v) in n {
                self.variables.insert(k.clone(), v.clone());
            }
        }
        saved
    }

    pub fn restore_vars(&mut self, saved: &Vars) {
        self.variables = saved.clone();
    }

    pub fn update_variables(&mut self, variables: &Vars) {
        for (k, v) in variables {
            self.variables.insert(k.clone(), v.clone());
        }
    }

    /// use `file` to get the mime type of a file; fallback on content sniffing
    fn get_filetype(&self, src: &str) -> String {
        if utils::which("file", None).is_some() {
            let (ok, out) = utils::run(&[
                "file".into(),
                "-L".into(),
                "-b".into(),
                "--mime-type".into(),
                src.into(),
            ]);
            if ok {
                dbg_log!("using \"file\" for filetype identification");
                return out.trim().to_string();
            }
        }
        match fs::read(src) {
            Ok(c) if c.is_empty() => "inode/x-empty".to_string(),
            Ok(c) if c.iter().take(8000).any(|b| *b == 0) => "application/octet-stream".to_string(),
            Ok(_) => "text/plain".to_string(),
            Err(_) => "application/octet-stream".to_string(),
        }
    }

    fn is_text(&self, fileoutput: &str) -> bool {
        let out = fileoutput.to_lowercase();
        if out.starts_with("text")
            || out.contains("empty")
            || out.contains("json")
            || out.contains("javascript")
            || out.contains("ecmascript")
        {
            return true;
        }
        if self.mime_text.contains(&out) {
            dbg_log!("mime type forced to \"text\" due to type");
            return true;
        }
        false
    }

    fn handle_text_file(&self, src: &str) -> Result<Vec<u8>> {
        let rel = utils::relpath(src, &self.base);
        let raw = fs::read(src)?;
        let data = String::from_utf8_lossy(&raw);
        self.env
            .render_named_str(&rel, &data, self.context())
            .map(String::into_bytes)
            .map_err(to_error)
    }

    fn handle_bin_file(&self, src: &str) -> Result<Vec<u8>> {
        let src = if src.starts_with(&self.base) {
            src.to_string()
        } else {
            utils::join(&self.base, src)
        };
        Ok(fs::read(src)?)
    }

    /// recursively check if any file is a template within path
    pub fn path_is_template(path: &str) -> bool {
        let path = utils::expanduser(path);
        if !utils::exists(&path) {
            dbg_log!("is NOT template: \"{path}\"");
            return false;
        }
        if utils::is_file(&path) {
            return Self::file_is_template(&path);
        }
        for entry in utils::listdir(&path) {
            let fpath = utils::join(&path, &entry);
            let found = if utils::is_file(&fpath) {
                Self::file_is_template(&fpath)
            } else {
                Self::path_is_template(&fpath)
            };
            if found {
                dbg_log!("is indeed template: \"{path}\"");
                return true;
            }
        }
        dbg_log!("is NOT template: \"{path}\"");
        false
    }

    pub fn string_is_template(s: &str) -> bool {
        s.contains(VAR_START)
    }

    /// check if a variable value (string or dict) contains template(s)
    pub fn var_is_template(v: &Value) -> bool {
        match v {
            Value::String(s) => Self::string_is_template(s),
            Value::Mapping(m) => Self::dict_is_template(m),
            _ => false,
        }
    }

    fn dict_is_template(m: &serde_yaml_ng::Mapping) -> bool {
        for (_, v) in m {
            match v {
                Value::String(s) => {
                    if Self::string_is_template(s) {
                        return true;
                    }
                }
                Value::Mapping(sub) => return Self::dict_is_template(sub),
                _ => {}
            }
        }
        false
    }

    fn file_is_template(path: &str) -> bool {
        dbg_log!("is template: {path}");
        if !utils::is_file(path) {
            return false;
        }
        match fs::read(path) {
            Ok(content) if !content.is_empty() => [BLOCK_START, VAR_START, COMMENT_START]
                .iter()
                .any(|m| contains(&content, m.as_bytes())),
            _ => false,
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
