//! Un-installation of dotfiles.

use crate::dbg_log;
use crate::linktype::LinkType;
use crate::log;
use crate::utils::{self, dir_empty, join, try_removepath};
use std::fs;

pub struct Uninstaller {
    dry: bool,
    safe: bool,
    backup_suffix: String,
}

impl Uninstaller {
    pub fn new(dry: bool, safe: bool, backup_suffix: &str) -> Self {
        Self {
            dry,
            safe,
            backup_suffix: backup_suffix.to_string(),
        }
    }

    /// uninstall dst; (true, None) on success, (false, error) otherwise
    pub fn uninstall(&self, src: &str, dst: &str, linktype: LinkType) -> (bool, Option<String>) {
        if src.is_empty() || dst.is_empty() {
            dbg_log!("cannot uninstall empty {src} or {dst}");
            return (true, None);
        }
        let path = utils::normpath(&utils::expanduser(dst));
        let path = path.trim_end_matches('/').to_string();
        if !utils::is_file(&path) && !utils::is_dir(&path) {
            return (false, Some(format!("cannot uninstall special file {path}")));
        }
        dbg_log!("uninstalling \"{path}\" (link: {linktype})");
        let (ret, msg) = self.remove(&path);
        if ret && !self.dry {
            log::sub(&format!("uninstall {dst}"));
        }
        (ret, msg)
    }

    fn descend(&self, dirpath: &str) -> (bool, Option<String>) {
        let mut ret = true;
        dbg_log!("recursively uninstall {dirpath}");
        for sub in utils::listdir(dirpath) {
            let subpath = join(dirpath, &sub);
            if utils::is_dir(&subpath) {
                self.descend(&subpath);
            } else {
                let (subret, _) = self.remove(&subpath);
                if !subret {
                    ret = false;
                }
            }
        }
        if dir_empty(dirpath) {
            dbg_log!("remove empty dir {dirpath}");
            if self.dry {
                log::dry(&format!("would \"rm -r {dirpath}\""));
                return (true, None);
            }
            return self.remove_path(dirpath);
        }
        dbg_log!("not removing non-empty dir {dirpath}");
        (ret, None)
    }

    fn remove_path(&self, path: &str) -> (bool, Option<String>) {
        match try_removepath(path) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(format!("removing \"{path}\" failed: {e}"))),
        }
    }

    fn remove(&self, path: &str) -> (bool, Option<String>) {
        dbg_log!("handling uninstall of {path}");
        if path.ends_with(&self.backup_suffix) {
            dbg_log!("skip {path} ignored");
            return (true, None);
        }
        let backup = format!("{path}{}", self.backup_suffix);
        if utils::exists(&backup) {
            dbg_log!("backup exists for {path}: {backup}");
            return self.replace(path, &backup);
        }
        if utils::is_dir(path) {
            dbg_log!("{path} is a directory");
            return self.descend(path);
        }
        if self.dry {
            log::dry(&format!("would \"rm {path}\""));
            return (true, None);
        }
        if self.safe && !log::ask(&format!("Remove {path}?")) {
            return (false, Some("user refused".into()));
        }
        dbg_log!("removing {path}");
        self.remove_path(path)
    }

    fn replace(&self, path: &str, backup: &str) -> (bool, Option<String>) {
        if self.dry {
            log::dry(&format!("would \"mv {backup} {path}\""));
            return (true, None);
        }
        if self.safe && !log::ask(&format!("Restore {path} from {backup}?")) {
            return (false, Some("user refused".into()));
        }
        dbg_log!("mv {backup} {path}");
        match fs::rename(backup, path) {
            Ok(()) => (true, None),
            Err(e) => (
                false,
                Some(format!("replacing \"{path}\" by \"{backup}\" failed: {e}")),
            ),
        }
    }
}
