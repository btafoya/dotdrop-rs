//! Comparison of dotfiles (dotdrop side) and deployed files.

use crate::dbg_log;
use crate::ftree::FTreeDir;
use crate::model::Chmod;
use crate::utils::{self, diff, get_file_perm, join, must_ignore};

pub struct Comparator {
    diff_cmd: String,
    ignore_missing_in_dotdrop: bool,
}

impl Comparator {
    pub fn new(diff_cmd: &str, ignore_missing_in_dotdrop: bool) -> Self {
        Self {
            diff_cmd: diff_cmd.to_string(),
            ignore_missing_in_dotdrop,
        }
    }

    /// diff local_path (dotdrop dotfile) and deployed_path (destination file);
    /// returns '' if same. If mode is None, rights are read from local_path
    pub fn compare(
        &self,
        local_path: &str,
        deployed_path: &str,
        ignore: &[String],
        mode: Option<Chmod>,
    ) -> String {
        let local_path = utils::expanduser(local_path);
        let deployed_path = utils::expanduser(deployed_path);
        dbg_log!("comparing \"{local_path}\" and \"{deployed_path}\"");
        dbg_log!("ignore pattern(s): {ignore:?}");
        self.compare_paths(&local_path, &deployed_path, ignore, mode, true)
    }

    fn compare_paths(
        &self,
        local_path: &str,
        deployed_path: &str,
        ignore: &[String],
        mode: Option<Chmod>,
        recurse: bool,
    ) -> String {
        if !utils::exists(local_path) {
            return format!("=> \"{local_path}\" does not exist on destination\n");
        }
        if !self.ignore_missing_in_dotdrop && !utils::exists(deployed_path) {
            return format!("=> \"{deployed_path}\" does not exist in dotdrop\n");
        }
        let (ldir, ddir) = (utils::is_dir(local_path), utils::is_dir(deployed_path));
        if ldir && !ddir {
            return format!("\"{local_path}\" is a dir while \"{deployed_path}\" is a file\n");
        }
        if !ldir && ddir {
            return format!("\"{local_path}\" is a file while \"{deployed_path}\" is a dir\n");
        }
        if !ldir {
            dbg_log!("{local_path} is a file");
            let ret = self.comp_file(local_path, deployed_path, ignore);
            if ret.is_empty() {
                return self.comp_mode(local_path, deployed_path, mode);
            }
            return ret;
        }
        dbg_log!("\"{local_path}\" is a directory");
        let mut ret = String::new();
        if recurse {
            ret = self.comp_dir(local_path, deployed_path, ignore);
        }
        if ret.is_empty() {
            ret = self.comp_mode(local_path, deployed_path, mode);
        }
        ret
    }

    fn comp_mode(&self, local_path: &str, deployed_path: &str, mode: Option<Chmod>) -> String {
        let local_mode = match mode {
            Some(Chmod::Mode(m)) if m != 0 => m,
            _ => get_file_perm(local_path),
        };
        let deployed_mode = get_file_perm(deployed_path);
        if local_mode == deployed_mode {
            return String::new();
        }
        dbg_log!(
            "mode differ {local_path} ({local_mode:o}) and {deployed_path} ({deployed_mode:o})"
        );
        format!("modes differ for {deployed_path} ({deployed_mode:o}) vs {local_mode:o}\n")
    }

    fn comp_file(&self, local_path: &str, deployed_path: &str, ignore: &[String]) -> String {
        dbg_log!("compare file {local_path} with {deployed_path}");
        if (self.ignore_missing_in_dotdrop && !utils::exists(local_path))
            || must_ignore(&[local_path, deployed_path], ignore, false)
        {
            dbg_log!("ignoring diff {local_path} and {deployed_path}");
            return String::new();
        }
        self.diff_files(local_path, deployed_path)
    }

    fn comp_dir(&self, local_path: &str, deployed_path: &str, ignore: &[String]) -> String {
        dbg_log!("compare directory {local_path} with {deployed_path}");
        if !utils::exists(deployed_path) {
            return String::new();
        }
        let ign_missing = self.ignore_missing_in_dotdrop && !utils::exists(local_path);
        if ign_missing || must_ignore(&[local_path, deployed_path], ignore, false) {
            dbg_log!("ignoring diff {local_path} and {deployed_path}");
            return String::new();
        }
        if !utils::is_dir(deployed_path) {
            return format!("\"{deployed_path}\" is a file\n");
        }
        self.compare_dirs(local_path, deployed_path, ignore)
    }

    fn compare_dirs(&self, local_path: &str, deployed_path: &str, ignore: &[String]) -> String {
        dbg_log!("compare dirs {local_path} and {deployed_path}");
        let mut ret = String::new();
        let local_tree = FTreeDir::new(local_path, ignore);
        let deploy_tree = FTreeDir::new(deployed_path, ignore);
        let (lonly, ronly, common) = local_tree.compare(&deploy_tree);
        for i in lonly {
            let path = join(local_path, &i);
            if utils::is_dir(&path) {
                continue;
            }
            ret += &format!("=> \"{path}\" does not exist on destination\n");
        }
        if !self.ignore_missing_in_dotdrop {
            for i in ronly {
                let path = join(deployed_path, &i);
                if utils::is_dir(&path) {
                    continue;
                }
                ret += &format!("=> \"{path}\" does not exist in dotdrop\n");
            }
        }
        dbg_log!("common files {common:?}");
        for i in common {
            let source_file = join(local_path, &i);
            let deployed_file = join(deployed_path, &i);
            ret += &self.compare_paths(&source_file, &deployed_file, &[], None, false);
        }
        ret
    }

    fn diff_files(&self, local_path: &str, deployed_path: &str) -> String {
        let out = diff(deployed_path, local_path, &self.diff_cmd);
        if out.is_empty() {
            return out;
        }
        format!("=> diff \"{}\":\n{out}", utils::basename(local_path))
    }
}
