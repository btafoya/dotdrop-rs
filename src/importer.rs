//! Import of dotfiles into the dotpath and the config.

use crate::cfg_aggregator::CfgAggregator;
use crate::comparator::Comparator;
use crate::dbg_log;
use crate::error::{Error, Result};
use crate::ftree::FTreeDir;
use crate::linktype::LinkType;
use crate::log;
use crate::model::Transform;
use crate::templategen::{Templategen, Vars};
use crate::utils::{
    self, copy2, get_default_file_perms, get_file_perm, get_umask, get_unique_tmp_name, join,
    must_ignore, removepath, strip_home,
};
use std::fs;

pub struct Importer {
    profile: String,
    dotpath: String,
    diff_cmd: String,
    dry: bool,
    safe: bool,
    keepdot: bool,
    ignore: Vec<String>,
    forcekey: Option<String>,
    templater: Templategen,
    umask: u32,
}

/// result of an import
#[derive(Debug, PartialEq, Eq)]
pub enum Imported {
    One,
    Ignored,
    Failed,
}

impl Importer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        profile: &str,
        dotpath: &str,
        diff_cmd: &str,
        variables: &Vars,
        dry: bool,
        safe: bool,
        keepdot: bool,
        ignore: &[String],
        forcekey: Option<String>,
    ) -> Result<Self> {
        if profile.is_empty() {
            return Err(Error::Undefined("profile is undefined".into()));
        }
        // patch ignore patterns
        let ignore = ignore
            .iter()
            .map(|ign| {
                if ign.starts_with('!') || ign.starts_with("*/") {
                    ign.clone()
                } else {
                    let new = format!("*/{ign}");
                    dbg_log!("patching ignore {ign} to {new}");
                    new
                }
            })
            .collect();
        Ok(Self {
            profile: profile.to_string(),
            dotpath: dotpath.to_string(),
            diff_cmd: diff_cmd.to_string(),
            dry,
            safe,
            keepdot,
            ignore,
            forcekey,
            templater: Templategen::new(dotpath, Some(variables), &[], &[]),
            umask: get_umask(),
        })
    }

    /// import a dotfile pointed by path
    pub fn import_path(
        &mut self,
        conf: &mut CfgAggregator,
        path: &str,
        import_as: Option<&str>,
        import_link: LinkType,
        import_mode: bool,
        trans_install: Option<&str>,
        trans_update: Option<&str>,
    ) -> Imported {
        let path = utils::abspath(path);
        dbg_log!("import {path}");
        if !utils::exists(&path) {
            log::err(format!("\"{path}\" does not exist, ignored!"));
            return Imported::Failed;
        }
        let tinstall = trans_install.and_then(|t| conf.get_trans_install(t));
        let tupdate = trans_update.and_then(|t| conf.get_trans_update(t));
        self.import(
            conf,
            &path,
            import_as,
            import_link,
            import_mode,
            tinstall.as_ref(),
            tupdate.as_ref(),
        )
    }

    fn import(
        &mut self,
        conf: &mut CfgAggregator,
        path: &str,
        import_as: Option<&str>,
        import_link: LinkType,
        import_mode: bool,
        trans_install: Option<&Transform>,
        trans_update: Option<&Transform>,
    ) -> Imported {
        let infspath = utils::abspath(path.trim_end_matches('/'));
        if self.ignored(&infspath) {
            return Imported::Ignored;
        }
        // ask confirmation for symlinks
        if self.safe {
            let realdst = utils::realpath(&infspath);
            if infspath != realdst {
                let msg = format!("\"{infspath}\" is a symlink, dereference it and continue?");
                if !log::ask(&msg) {
                    return Imported::Ignored;
                }
            }
        }

        // create src path
        let mut indotpath = strip_home(&infspath);
        if let Some(a) = import_as {
            let p = utils::expanduser(a);
            let p = utils::abspath(p.trim_end_matches('/'));
            indotpath = strip_home(&p);
            dbg_log!("import src for {infspath} as {indotpath}");
        }
        // with or without dot prefix
        let strip: &[char] = if self.keepdot { &['/'] } else { &['.', '/'] };
        let indotpath = indotpath.trim_start_matches(strip).to_string();

        let perm = get_file_perm(&infspath);
        let linktype = import_link;
        if linktype == LinkType::LinkChildren && !utils::is_dir(path) {
            log::err(format!("importing \"{path}\" failed!"));
            return Imported::Failed;
        }
        if self.already_exists(conf, &indotpath, &infspath) {
            return Imported::Failed;
        }

        dbg_log!("import dotfile: src:{indotpath} dst:{infspath}");
        if !self.import_to_dotpath(&indotpath, &infspath, trans_update) {
            dbg_log!("import files failed");
            return Imported::Failed;
        }
        self.import_in_config(
            conf,
            path,
            &indotpath,
            &infspath,
            perm,
            linktype,
            import_mode,
            trans_install,
            trans_update,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn import_in_config(
        &self,
        conf: &mut CfgAggregator,
        path: &str,
        src: &str,
        dst: &str,
        perm: u32,
        linktype: LinkType,
        import_mode: bool,
        trans_install: Option<&Transform>,
        trans_update: Option<&Transform>,
    ) -> Imported {
        let mut chmod = None;
        let dflperm = get_default_file_perms(dst, self.umask);
        dbg_log!("import chmod: {import_mode}");
        if import_mode || perm != dflperm {
            dbg_log!("adopt mode {perm:o} (umask {dflperm:o})");
            chmod = Some(perm);
        }
        let ok = conf
            .new_dotfile(
                src,
                dst,
                linktype,
                chmod,
                trans_install,
                trans_update,
                self.forcekey.as_deref(),
            )
            .unwrap_or_else(|e| {
                log::err(e);
                false
            });
        if !ok {
            log::warn(format!("\"{path}\" ignored during import"));
            return Imported::Ignored;
        }
        log::sub(&format!("\"{path}\" imported"));
        Imported::One
    }

    /// check if a dotfile in the dotpath already exists for this src
    fn check_existing_dotfile(&self, src: &str, dst: &str) -> bool {
        if !utils::exists(src) || !self.safe {
            return true;
        }
        let cmp = Comparator::new(&self.diff_cmd, false);
        let diff = cmp.compare(src, dst, &[], None);
        if !diff.is_empty() {
            log::log(&format!("diff \"{dst}\" VS \"{src}\""));
            log::emph(&diff);
            if !log::ask(&format!("Dotfile \"{src}\" already exists, overwrite?")) {
                return false;
            }
            dbg_log!("will overwrite existing file");
        }
        true
    }

    fn import_to_dotpath(
        &mut self,
        in_dotpath: &str,
        in_fs: &str,
        trans_update: Option<&Transform>,
    ) -> bool {
        let in_dotpath_abs = join(&self.dotpath, in_dotpath);
        if !self.check_existing_dotfile(&in_dotpath_abs, in_fs) {
            dbg_log!("{in_dotpath_abs} exits already");
            return false;
        }
        if self.dry {
            log::dry(&format!("would copy {in_fs} to {in_dotpath_abs}"));
            return true;
        }
        let Some(in_fs) = self.apply_trans_update(in_fs, trans_update) else {
            return false;
        };
        if !utils::is_dir(&in_fs) {
            self.import_file_to_dotpath(&in_fs, &in_dotpath_abs);
        }
        let fstree = FTreeDir::new(&in_fs, &self.ignore);
        dbg_log!("{} files to import", fstree.entries().len());
        for entry in fstree.entries() {
            dbg_log!("importing {entry}...");
            let rel_src = utils::relpath(entry, &in_fs);
            let dst = join(&in_dotpath_abs, &rel_src);
            if utils::is_dir(entry) {
                // directories are created based on files
                continue;
            }
            self.import_file_to_dotpath(entry, &dst);
        }
        utils::exists(&in_dotpath_abs)
    }

    fn import_file_to_dotpath(&self, src: &str, dst: &str) -> bool {
        dbg_log!("importing {src} to {dst}");
        let _ = fs::create_dir_all(utils::dirname(dst));
        if let Err(e) = copy2(src, dst) {
            log::err(format!("importing \"{src}\" failed: {e}"));
            return false;
        }
        true
    }

    /// no other dotfile with same dst but different src exists for this profile
    fn already_exists(&self, conf: &CfgAggregator, src: &str, dst: &str) -> bool {
        let dfs = conf.get_dotfile_by_dst(dst, Some(&self.profile));
        for dotfile in dfs {
            let in_profile = conf
                .get_profiles_by_dotfile_key(&dotfile.key)
                .iter()
                .any(|p| p.key == self.profile);
            if in_profile && conf.get_dotfile_by_src_dst(src, dst).is_none() {
                log::err(format!("duplicate dotfile: {}", dotfile.key));
                return true;
            }
        }
        false
    }

    fn ignored(&self, path: &str) -> bool {
        if must_ignore(&[path], &self.ignore, false) {
            dbg_log!("ignoring import of {path}");
            log::warn(format!("{path} ignored"));
            return true;
        }
        false
    }

    /// apply transformation to path; the new path (tmp file) if trans, the
    /// original path if no trans, None on error
    fn apply_trans_update(&self, path: &str, trans: Option<&Transform>) -> Option<String> {
        let Some(trans) = trans else {
            return Some(path.to_string());
        };
        dbg_log!("executing write transformation {trans}");
        let tmp = get_unique_tmp_name();
        if !trans.transform(path, &tmp, Some(&self.templater)) {
            log::err(format!(
                "transformation \"{}\" failed for {path}",
                trans.key
            ));
            if utils::exists(&tmp) {
                removepath(&tmp);
            }
            return None;
        }
        Some(tmp)
    }
}
