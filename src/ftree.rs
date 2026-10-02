//! Filesystem tree of a directory used for comparison.

use crate::dbg_log;
use crate::utils::{self, dir_empty, join, must_ignore};
use std::collections::BTreeSet;

pub struct FTreeDir {
    pub path: String,
    entries: Vec<String>,
}

impl FTreeDir {
    pub fn new(path: &str, ignores: &[String]) -> Self {
        let mut me = Self {
            path: path.to_string(),
            entries: Vec::new(),
        };
        if utils::is_dir(path) {
            me.walk(ignores);
        }
        me
    }

    /// index directory, ignore empty directories and ignored patterns
    fn walk(&mut self, ignores: &[String]) {
        for (root, dirs, files) in utils::walk(&self.path, true) {
            for file in files {
                let fpath = join(&root, &file);
                if must_ignore(&[&fpath], ignores, true) {
                    dbg_log!("ignoring file {fpath}");
                    continue;
                }
                dbg_log!("added file to list of {}: {fpath}", self.path);
                self.entries.push(fpath);
            }
            for dname in dirs {
                let dpath = join(&root, &dname);
                if dir_empty(&dpath) {
                    dbg_log!("ignoring empty dir {dpath}");
                    continue;
                }
                // a trailing "/" allows patterns like "*/dir/*" to match the directory itself
                let dpath = format!("{dpath}/");
                if must_ignore(&[&dpath], ignores, true) {
                    dbg_log!("ignoring dir {dpath}");
                    continue;
                }
                dbg_log!("added dir to list of {}: {dpath}", self.path);
                self.entries.push(dpath);
            }
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// returns (left only, right only, in both) relative paths
    pub fn compare(&self, other: &FTreeDir) -> (Vec<String>, Vec<String>, Vec<String>) {
        let rel = |t: &FTreeDir| -> BTreeSet<String> {
            t.entries
                .iter()
                .map(|e| utils::relpath(e, &t.path))
                .collect()
        };
        let (left, right) = (rel(self), rel(other));
        (
            left.difference(&right).cloned().collect(),
            right.difference(&left).cloned().collect(),
            left.intersection(&right).cloned().collect(),
        )
    }
}
