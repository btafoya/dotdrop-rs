//! Update of dotfiles in the dotpath from their deployed version.

use crate::cfg_aggregator::CfgAggregator;
use crate::dbg_log;
use crate::ftree::FTreeDir;
use crate::linktype::LinkType;
use crate::log;
use crate::model::Dotfile;
use crate::templategen::{Templategen, Vars};
use crate::utils::{
    self, copy2, diff, get_file_perm, get_unique_tmp_name, ignores_to_absolute, join,
    mirror_file_rights, must_ignore, removepath, same_content, write_to_tmpfile,
};
use std::fs;

pub struct Updater {
    dotpath: String,
    profile_key: String,
    dry: bool,
    safe: bool,
    ignore: Vec<String>,
    showpatch: bool,
    ignore_missing_in_dotdrop: bool,
    templater: Templategen,
    tvars: Vars,
}

impl Updater {
    pub fn new(
        dotpath: &str,
        variables: &Vars,
        profile_key: &str,
        dry: bool,
        safe: bool,
        ignore: Vec<String>,
        showpatch: bool,
        ignore_missing_in_dotdrop: bool,
    ) -> Self {
        let mut templater = Templategen::new(dotpath, Some(variables), &[], &[]);
        let tvars = templater.add_tmp_vars(None);
        Self {
            dotpath: dotpath.to_string(),
            profile_key: profile_key.to_string(),
            dry,
            safe,
            ignore,
            showpatch,
            ignore_missing_in_dotdrop,
            templater,
            tvars,
        }
    }

    /// update the dotfile installed on path
    pub fn update_path(&mut self, conf: &mut CfgAggregator, path: &str) -> bool {
        let path = utils::expanduser(path);
        if !utils::lexists(&path) {
            log::err(format!("\"{path}\" does not exist!"));
            return false;
        }
        let dotfiles = conf.get_dotfile_by_dst(&path, Some(&self.profile_key));
        if dotfiles.is_empty() {
            return false;
        }
        for dotfile in dotfiles {
            dbg_log!("updating {dotfile} from path \"{path}\"");
            if !self.update(conf, &path, &dotfile) {
                return false;
            }
        }
        true
    }

    /// update the dotfile referenced by key
    pub fn update_key(&mut self, conf: &mut CfgAggregator, key: &str) -> bool {
        let Some(dotfile) = conf.get_dotfile(key, Some(&self.profile_key)) else {
            log::err(format!("invalid dotfile for update: {key}"));
            return false;
        };
        dbg_log!("updating {dotfile} from key \"{key}\"");
        let path = conf.path_to_dotfile_dst(&dotfile.dst);
        self.update(conf, &path, &dotfile)
    }

    fn update(&mut self, conf: &mut CfgAggregator, path: &str, dotfile: &Dotfile) -> bool {
        let mut ignores = self.ignore.clone();
        for i in &dotfile.upignore {
            if !ignores.contains(i) {
                ignores.push(i.clone());
            }
        }
        let ignores = ignores_to_absolute(&ignores, &[&dotfile.dst, &dotfile.src]);
        dbg_log!("ignore pattern(s) for {path}: {ignores:?}");

        let deployed_path = utils::expanduser(path);
        let local_path = utils::expanduser(&join(&self.dotpath, &dotfile.src));
        if !utils::exists(&deployed_path) {
            log::err(format!("\"{deployed_path}\" does not exist"));
            return false;
        }
        if !utils::exists(&local_path) {
            log::err(format!("\"{local_path}\" does not exist, import it first"));
            return false;
        }

        // apply write transformation if any
        let Some(new_path) = self.apply_trans_update(&deployed_path, dotfile) else {
            return false;
        };

        // save current rights
        let deployed_mode = get_file_perm(&deployed_path);
        let local_mode = get_file_perm(&local_path);

        let mut ret = if utils::is_dir(&new_path) {
            self.handle_dir(&new_path, &local_path, dotfile, &ignores)
        } else {
            self.handle_file(&new_path, &local_path, &ignores)
        };
        if !ret {
            return false;
        }

        // mirror rights
        if deployed_mode != local_mode {
            dbg_log!("adopt mode {deployed_mode:o} for {}", dotfile.key);
            if conf
                .update_dotfile(&dotfile.key, Some(deployed_mode))
                .unwrap_or(false)
            {
                ret = true;
            }
            self.mirror_file_perms(&deployed_path, &local_path);
        }

        // clean temporary files
        if new_path != deployed_path && utils::exists(&new_path) {
            removepath(&new_path);
        }
        ret
    }

    /// apply write transformation to dotfile; None on error
    fn apply_trans_update(&mut self, path: &str, dotfile: &Dotfile) -> Option<String> {
        let Some(trans) = &dotfile.trans_update else {
            return Some(path.to_string());
        };
        dbg_log!("executing write transformation {trans}");
        let tmp = get_unique_tmp_name();
        self.templater.restore_vars(&self.tvars);
        self.templater
            .add_tmp_vars(Some(&dotfile.dotfile_variables()));
        if !trans.transform(path, &tmp, Some(&self.templater)) {
            if utils::exists(&tmp) {
                removepath(&tmp);
            }
            log::err(format!(
                "transformation \"{}\" failed for {}",
                trans.key, dotfile.key
            ));
            return None;
        }
        Some(tmp)
    }

    fn is_template(&self, path: &str) -> bool {
        if !Templategen::path_is_template(path) {
            dbg_log!("{path} is NO template");
            return false;
        }
        log::warn(format!("{path} uses template, update manually"));
        true
    }

    /// provide a way to manually patch the template
    fn show_patch(&mut self, fpath: &str, tpath: &str) {
        self.templater.restore_vars(&self.tvars);
        match self.templater.generate(tpath) {
            Ok(content) => {
                if let Ok(tmp) = write_to_tmpfile(&content) {
                    let _ = mirror_file_rights(tpath, &tmp);
                    log::warn(format!(
                        "try patching with: \"diff -u {tmp} {fpath} | patch {tpath}\""
                    ));
                }
            }
            Err(e) => log::warn(format!("unable to show patch for {fpath}: {e}")),
        }
    }

    fn same_rights(left: &str, right: &str) -> bool {
        get_file_perm(left) == get_file_perm(right)
    }

    fn mirror_file_perms(&self, src: &str, dst: &str) {
        let (srcr, dstr) = (get_file_perm(src), get_file_perm(dst));
        if srcr == dstr {
            return;
        }
        dbg_log!("copy rights from {src} ({srcr:o}) to {dst} ({dstr:o})");
        if let Err(e) = mirror_file_rights(src, dst) {
            log::err(e);
        }
    }

    fn handle_file(&mut self, deployed_path: &str, local_path: &str, ignores: &[String]) -> bool {
        if self.must_ignore(&[deployed_path, local_path], ignores) {
            log::sub(&format!("\"{local_path}\" ignored"));
            return true;
        }
        dbg_log!("update for file {deployed_path} and {local_path}");
        if self.is_template(local_path) {
            dbg_log!("{local_path} is a template");
            if self.showpatch {
                self.show_patch(deployed_path, local_path);
            }
            return false;
        }
        if same_content(deployed_path, local_path) && Self::same_rights(deployed_path, local_path) {
            dbg_log!("identical files: {deployed_path} and {local_path}");
            return true;
        }
        if !self.overwrite(deployed_path, local_path) {
            return false;
        }
        if self.dry {
            log::dry(&format!("would cp {deployed_path} {local_path}"));
            return true;
        }
        dbg_log!("cp {deployed_path} {local_path}");
        match copy2(deployed_path, local_path) {
            Ok(()) => {
                log::sub(&format!("\"{local_path}\" updated"));
                true
            }
            Err(e) => {
                log::warn(format!("{deployed_path} update failed, do manually: {e}"));
                false
            }
        }
    }

    /// return only top-most paths to avoid redundant removals
    fn prune_nested_paths(entries: &[String]) -> Vec<String> {
        let mut normalized: Vec<String> = entries
            .iter()
            .map(|e| e.trim_end_matches('/').to_string())
            .collect();
        normalized.sort_by_key(|x| x.matches('/').count());
        let mut kept: Vec<String> = Vec::new();
        for entry in normalized {
            if kept
                .iter()
                .any(|k| entry == *k || entry.starts_with(&format!("{k}/")))
            {
                continue;
            }
            kept.push(entry);
        }
        kept
    }

    fn handle_dir(
        &mut self,
        deployed_path: &str,
        local_path: &str,
        dotfile: &Dotfile,
        ignores: &[String],
    ) -> bool {
        if dotfile.link == LinkType::LinkChildren {
            return self.handle_link_children_dir(deployed_path, local_path, dotfile, ignores);
        }
        let mut ret = true;
        dbg_log!("handle update for dir {deployed_path} to {local_path}");
        let deployed_path = utils::expanduser(deployed_path);
        let local_path = utils::expanduser(local_path);

        let local_tree = FTreeDir::new(&local_path, ignores);
        let deploy_tree = FTreeDir::new(&deployed_path, ignores);
        let (lonly, ronly, common) = local_tree.compare(&deploy_tree);

        // those only in dotpath
        for i in Self::prune_nested_paths(&lonly) {
            let path = join(&local_path, &i);
            if self.dry {
                log::dry(&format!("would rm -r {path}"));
                continue;
            }
            dbg_log!("rm -r {path}");
            if !self.confirm_rm_r(&path) {
                continue;
            }
            if !removepath(&path) {
                log::warn(format!("unable to remove {path}, do manually"));
                ret = false;
                continue;
            }
            log::sub(&format!("\"{path}\" removed"));
        }

        let ignore_missing = self.ignore_missing_in_dotdrop || dotfile.ignore_missing_in_dotdrop;
        if !ignore_missing {
            for i in &ronly {
                let srcpath = join(&deployed_path, i);
                let dstpath = join(&local_path, i);
                if self.dry {
                    log::dry(&format!("would cp -r {srcpath} {dstpath}"));
                    continue;
                }
                dbg_log!("cp {srcpath} {dstpath}");
                if !utils::is_dir(&srcpath) {
                    let _ = fs::create_dir_all(utils::dirname(&dstpath));
                    if let Err(e) = copy2(&srcpath, &dstpath) {
                        log::warn(format!(
                            "{srcpath} update right only failed, do manually: {e}"
                        ));
                        ret = false;
                        continue;
                    }
                }
                log::sub(&format!("\"{dstpath}\" updated"));
            }
        }

        for i in &common {
            let srcpath = join(&deployed_path, i);
            let dstpath = join(&local_path, i);
            if !self.sync_common_file(&srcpath, &dstpath) {
                ret = false;
            }
        }
        ret
    }

    /// sync a file present in both trees; false on failure
    fn sync_common_file(&mut self, srcpath: &str, dstpath: &str) -> bool {
        if utils::is_dir(srcpath) {
            return true;
        }
        if self.is_template(dstpath) {
            dbg_log!("{dstpath} is a template");
            if self.showpatch {
                self.show_patch(srcpath, dstpath);
            }
            return true;
        }
        if !Self::same_rights(dstpath, srcpath) {
            self.mirror_file_perms(srcpath, dstpath);
        }
        if diff(srcpath, dstpath, "").is_empty() {
            return true;
        }
        if self.dry {
            log::dry(&format!("would update content of {dstpath} from {srcpath}"));
            return true;
        }
        dbg_log!("cp {srcpath} {dstpath}");
        if let Err(e) = copy2(srcpath, dstpath) {
            log::warn(format!("{srcpath} update common failed, do manually: {e}"));
            return false;
        }
        self.mirror_file_perms(srcpath, dstpath);
        log::sub(&format!("\"{dstpath}\" content updated"));
        true
    }

    /// sync link_children dotfile without traversing managed symlinks
    fn handle_link_children_dir(
        &mut self,
        deployed_path: &str,
        local_path: &str,
        dotfile: &Dotfile,
        ignores: &[String],
    ) -> bool {
        let mut ret = true;
        let deployed_path = utils::expanduser(deployed_path);
        let local_path = utils::expanduser(local_path);
        dbg_log!("handling update for link_children dotfile");

        let local_children = utils::listdir(&local_path);
        let deployed_children = utils::listdir(&deployed_path);
        let local_only: Vec<&String> = local_children
            .iter()
            .filter(|c| !deployed_children.contains(c))
            .collect();
        let deployed_only: Vec<&String> = deployed_children
            .iter()
            .filter(|c| !local_children.contains(c))
            .collect();
        let common: Vec<&String> = local_children
            .iter()
            .filter(|c| deployed_children.contains(c))
            .collect();

        for child in local_only {
            let path = join(&local_path, child);
            if self.must_ignore(&[&path], ignores) {
                log::sub(&format!("\"{path}\" ignored"));
                continue;
            }
            if self.dry {
                log::dry(&format!("would rm -r {path}"));
                continue;
            }
            dbg_log!("rm -r {path}");
            if !self.confirm_rm_r(&path) {
                continue;
            }
            if !removepath(&path) {
                log::warn(format!("unable to remove {path}, do manually"));
                ret = false;
                continue;
            }
            log::sub(&format!("\"{path}\" removed"));
        }

        let ignore_missing = self.ignore_missing_in_dotdrop || dotfile.ignore_missing_in_dotdrop;
        if !ignore_missing {
            for child in deployed_only {
                let srcpath = join(&deployed_path, child);
                let dstpath = join(&local_path, child);
                if self.must_ignore(&[&srcpath, &dstpath], ignores) {
                    log::sub(&format!("\"{dstpath}\" ignored"));
                    continue;
                }
                if self.dry {
                    if utils::is_link(&srcpath) {
                        let target = fs::read_link(&srcpath)
                            .map(|t| utils::os(t.as_os_str()))
                            .unwrap_or_default();
                        log::dry(&format!("would ln -s {target} {dstpath}"));
                    } else {
                        log::dry(&format!("would cp -r {srcpath} {dstpath}"));
                    }
                    continue;
                }
                dbg_log!("cp {srcpath} {dstpath}");
                let _ = fs::create_dir_all(utils::dirname(&dstpath));
                let res = if utils::is_link(&srcpath) {
                    fs::read_link(&srcpath).and_then(|t| std::os::unix::fs::symlink(t, &dstpath))
                } else if !utils::is_dir(&srcpath) {
                    copy2(&srcpath, &dstpath)
                } else {
                    Ok(())
                };
                if let Err(e) = res {
                    log::warn(format!(
                        "{srcpath} update right only failed, do manually: {e}"
                    ));
                    ret = false;
                    continue;
                }
                log::sub(&format!("\"{dstpath}\" updated"));
            }
        }

        for child in common {
            let srcpath = join(&deployed_path, child);
            let dstpath = join(&local_path, child);
            if self.must_ignore(&[&srcpath, &dstpath], ignores) {
                log::sub(&format!("\"{dstpath}\" ignored"));
                continue;
            }
            if utils::is_link(&srcpath) {
                // managed link_children symlink, no update required
                if utils::realpath(&srcpath) == utils::realpath(&dstpath) {
                    continue;
                }
                let target = fs::read_link(&srcpath)
                    .map(|t| utils::os(t.as_os_str()))
                    .unwrap_or_default();
                if self.dry {
                    log::dry(&format!("would replace {dstpath} with symlink {target}"));
                    continue;
                }
                if !self.overwrite(&srcpath, &dstpath) {
                    continue;
                }
                if !removepath(&dstpath) {
                    log::warn(format!("unable to remove {dstpath}, do manually"));
                    ret = false;
                    continue;
                }
                let _ = std::os::unix::fs::symlink(&target, &dstpath);
                log::sub(&format!("\"{dstpath}\" updated"));
                continue;
            }
            if !self.sync_common_file(&srcpath, &dstpath) {
                ret = false;
            }
        }
        ret
    }

    fn overwrite(&self, src: &str, dst: &str) -> bool {
        !(self.safe && !log::ask(&format!("Overwrite \"{dst}\" with \"{src}\"?")))
    }

    fn confirm_rm_r(&self, directory: &str) -> bool {
        !(self.safe && !log::ask(&format!("Recursively remove \"{directory}\"?")))
    }

    fn must_ignore(&self, paths: &[&str], ignores: &[String]) -> bool {
        if must_ignore(paths, ignores, false) {
            dbg_log!("ignoring update for {paths:?}");
            return true;
        }
        false
    }
}
