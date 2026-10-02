//! Installation of dotfiles.

use crate::dbg_log;
use crate::linktype::LinkType;
use crate::log;
use crate::model::Chmod;
use crate::templategen::{Templategen, Vars};
use crate::utils::{
    self, content_empty, copyfile, copytree, get_file_perm, is_dir, join, lexists, must_ignore,
    pivot_path, removepath, samefile, write_to_tmpfile,
};
use serde_yaml_ng::Value;
use std::fs;
use std::io::Write;

/// (installed, error): (true, None) success, (false, Some) error,
/// (false, None) ignored, (false, Some("aborted")) user aborted
pub type Ret = (bool, Option<String>);

/// callback executing the pre-actions
pub type ActionExec<'a> = &'a dyn Fn() -> Ret;

/// per install call parameters
#[derive(Clone, Copy)]
pub struct Spec<'a> {
    pub noempty: bool,
    pub ignore: &'a [String],
    pub is_template: bool,
    pub chmod: Option<Chmod>,
    pub dir_as_block: &'a [String],
}

pub struct Installer {
    pub create: bool,
    pub backup: bool,
    pub dry: bool,
    pub safe: bool,
    workdir: String,
    base: String,
    pub diff: bool,
    totemp: Option<String>,
    showdiff: bool,
    backup_suffix: String,
    diff_cmd: String,
    action_executed: bool,
    remove_existing_in_dir: bool,
    force_chmod: bool,
    /// avoids printing file copied logs when using install_to_temp for comparing
    comparing: bool,
}

pub struct InstallerOpts {
    pub base: String,
    pub create: bool,
    pub backup: bool,
    pub dry: bool,
    pub safe: bool,
    pub workdir: String,
    pub diff: bool,
    pub totemp: Option<String>,
    pub showdiff: bool,
    pub backup_suffix: String,
    pub diff_cmd: String,
    pub remove_existing_in_dir: bool,
    pub force_chmod: bool,
}

impl Installer {
    pub fn new(o: InstallerOpts) -> Self {
        Self {
            create: o.create,
            backup: o.backup,
            dry: o.dry,
            safe: o.safe,
            workdir: utils::normpath(&utils::expanduser(&o.workdir)),
            base: utils::normpath(&utils::expanduser(&o.base)),
            diff: o.diff,
            totemp: o.totemp,
            showdiff: o.showdiff,
            backup_suffix: o.backup_suffix,
            diff_cmd: o.diff_cmd,
            action_executed: false,
            remove_existing_in_dir: o.remove_existing_in_dir,
            force_chmod: o.force_chmod,
            comparing: false,
        }
    }

    ////////////////////////////////////////////////////////
    // public methods
    ////////////////////////////////////////////////////////

    /// install src to dst
    pub fn install(
        &mut self,
        templater: &mut Templategen,
        src: &str,
        dst: &str,
        linktype: LinkType,
        actionexec: Option<ActionExec>,
        spec: Spec,
    ) -> Ret {
        if src.is_empty() || dst.is_empty() {
            dbg_log!("fake dotfile installed");
            self.exec_pre_actions(actionexec);
            return (true, None);
        }
        dbg_log!("installing \"{src}\" to \"{dst}\" (link: {linktype})");
        let Some((src, dst)) = self.check_paths(src, dst) else {
            return self.log_install(false, Some(format!("empty dst or src for {src}")));
        };
        let src = join(&self.base, &src);
        if !utils::exists(&src) {
            return self.log_install(false, Some(format!("source dotfile does not exist: {src}")));
        }
        self.action_executed = false;

        // install to temporary dir and ignore any actions
        if let Some(totemp) = self.totemp.clone() {
            let (ret, err, _) = self.install_to_temp(templater, &totemp, &src, &dst, spec, false);
            return self.log_install(ret, err);
        }

        let isdir = is_dir(&src);
        dbg_log!("install {src} to {dst}");
        dbg_log!("\"{src}\" is a directory: {isdir}");
        let treat_as_block = isdir
            && linktype == LinkType::NoLink
            && self.must_treat_dir_as_block(&src, spec.dir_as_block);
        dbg_log!(
            "dir_as_block patterns: {:?}, treat_as_block: {treat_as_block}",
            spec.dir_as_block
        );
        if treat_as_block {
            let (ret, err, _) = self.copy_dir(templater, &src, &dst, actionexec, spec, true);
            return self.log_install(ret, err);
        }

        let (ret, err) = match linktype {
            LinkType::NoLink => {
                if isdir {
                    let (ret, err, ins) =
                        self.copy_dir(templater, &src, &dst, actionexec, spec, false);
                    if self.remove_existing_in_dir && !ins.is_empty() {
                        self.remove_existing_in_dir(&dst, &ins);
                    }
                    (ret, err)
                } else {
                    self.copy_file(templater, &src, &dst, actionexec, spec)
                }
            }
            LinkType::Link | LinkType::Absolute => {
                self.link_dotfile(templater, &src, &dst, actionexec, spec, true)
            }
            LinkType::Relative => self.link_dotfile(templater, &src, &dst, actionexec, spec, false),
            LinkType::LinkChildren => {
                if !isdir {
                    dbg_log!("symlink children of {src} to {dst}");
                    (
                        false,
                        Some(format!("source dotfile is not a directory: {src}")),
                    )
                } else {
                    self.link_children(templater, &src, &dst, actionexec, spec)
                }
            }
        };

        if self.dry {
            return self.log_install(ret, err);
        }
        self.apply_chmod_after_install(&src, &dst, ret, &err, spec.chmod, false, linktype);
        self.log_install(ret, err)
    }

    fn must_treat_dir_as_block(&self, path: &str, patterns: &[String]) -> bool {
        !patterns.is_empty() && must_ignore(&[path], patterns, true)
    }

    /// handle chmod after install
    #[allow(clippy::too_many_arguments)]
    fn apply_chmod_after_install(
        &mut self,
        src: &str,
        dst: &str,
        ret: bool,
        err: &Option<String>,
        chmod: Option<Chmod>,
        is_sub: bool,
        linktype: LinkType,
    ) {
        let mut apply = matches!(linktype, LinkType::NoLink | LinkType::LinkChildren)
            && utils::exists(dst)
            && (ret || err.is_none())
            && chmod != Some(Chmod::Preserve);
        let chmod = if is_sub { None } else { chmod };
        if !apply {
            dbg_log!("no chmod applied");
            return;
        }
        let mode = match chmod {
            Some(Chmod::Mode(m)) if m != 0 => m,
            _ => {
                let m = get_file_perm(src);
                dbg_log!("dotfile in dotpath perm: {m:o}");
                m
            }
        };
        dbg_log!("applying chmod {mode:o} to {dst}");
        if get_file_perm(dst) != mode {
            let msg = format!("chmod {dst} to {mode:o}");
            if !self.force_chmod && self.safe && !log::ask(&msg) {
                apply = false;
            } else {
                if !self.comparing {
                    log::sub(&msg);
                }
                if !utils::chmod(dst, mode) {
                    log::warn("chmod failed");
                }
            }
        }
        let _ = apply;
    }

    /// install a dotfile to a tempdir; returns (success, error, installed path)
    pub fn install_to_temp(
        &mut self,
        templater: &mut Templategen,
        tmpdir: &str,
        src: &str,
        dst: &str,
        spec: Spec,
        set_create: bool,
    ) -> (bool, Option<String>, Option<String>) {
        dbg_log!("tmp install {src} (defined dst: {dst})");
        let Some((src, dst)) = self.check_paths(src, dst) else {
            let err = format!("empty dst or src for {src}");
            self.log_install(false, Some(err.clone()));
            return (false, Some(err), None);
        };

        // save flags
        self.comparing = true;
        let drysaved = self.dry;
        self.dry = false;
        let diffsaved = self.diff;
        self.diff = false;
        let createsaved = self.create;
        if set_create {
            self.create = true;
        }
        let totemp = self.totemp.take();

        let tmpdst = pivot_path(&dst, tmpdir, false);
        let (ret, err) = self.install(templater, &src, &tmpdst, LinkType::NoLink, None, spec);
        if ret {
            dbg_log!("tmp installed in {tmpdst}");
        }

        // restore flags
        self.dry = drysaved;
        self.diff = diffsaved;
        self.create = createsaved;
        self.comparing = false;
        self.totemp = totemp;
        (ret, err, Some(tmpdst))
    }

    ////////////////////////////////////////////////////////
    // links
    ////////////////////////////////////////////////////////

    fn link_dotfile(
        &mut self,
        templater: &mut Templategen,
        src: &str,
        dst: &str,
        actionexec: Option<ActionExec>,
        spec: Spec,
        absolute: bool,
    ) -> Ret {
        let mut src = src.to_string();
        if spec.is_template {
            dbg_log!("is a template, installing to {}", self.workdir);
            let tmp = pivot_path(dst, &self.workdir, true);
            let sub_spec = Spec {
                noempty: false,
                dir_as_block: &[],
                ..spec
            };
            let (ret, err) = self.install(
                templater,
                &src,
                &tmp,
                LinkType::NoLink,
                actionexec,
                sub_spec,
            );
            if !ret && !utils::exists(&tmp) {
                return (ret, err);
            }
            src = tmp;
        }
        self.symlink(&src, dst, actionexec, absolute)
    }

    fn link_children(
        &mut self,
        templater: &mut Templategen,
        src: &str,
        dst: &str,
        mut actionexec: Option<ActionExec>,
        spec: Spec,
    ) -> Ret {
        let parent = join(&self.base, src);
        if !lexists(dst) {
            if self.dry {
                log::dry(&format!("would create directory \"{dst}\""));
            } else {
                if !self.comparing {
                    log::sub(&format!("creating directory \"{dst}\""));
                }
                self.create_dirs(dst);
            }
        }
        if utils::is_file(dst) {
            let msg = format!("Remove regular file {dst} and replace with empty directory?");
            if self.safe && !log::ask(&msg) {
                return (false, Some("aborted".into()));
            }
            let _ = fs::remove_file(dst);
            self.create_dirs(dst);
        }

        let children = utils::listdir(&parent);
        if self.remove_existing_in_dir {
            self.remove_stale_link_children(&parent, dst);
        }
        let mut installed = 0;
        for child in &children {
            let mut subsrc = utils::normpath(&join(&parent, child));
            let subdst = utils::normpath(&join(dst, child));
            if must_ignore(&[&subsrc, &subdst], spec.ignore, false) {
                dbg_log!("ignoring install of {src} to {dst}");
                continue;
            }
            dbg_log!("symlink child {subsrc} to {subdst}");
            if spec.is_template {
                dbg_log!(
                    "child is a template, install to {} and symlink",
                    self.workdir
                );
                let tmp = pivot_path(&subdst, &self.workdir, true);
                let sub_spec = Spec {
                    chmod: None,
                    dir_as_block: &[],
                    noempty: false,
                    ..spec
                };
                let (ret2, err2) = self.install(
                    templater,
                    &subsrc,
                    &tmp,
                    LinkType::NoLink,
                    actionexec,
                    sub_spec,
                );
                if !ret2 && err2.is_some() && !utils::exists(&tmp) {
                    continue;
                }
                subsrc = tmp;
            }
            let (ret, err) = self.symlink(&subsrc, &subdst, actionexec, true);
            if ret {
                installed += 1;
                // void actionexec if dotfile installed to prevent running actions multiple times
                actionexec = None;
            } else if err.is_some() {
                return (ret, err);
            }
        }
        (installed > 0, None)
    }

    ////////////////////////////////////////////////////////
    // file operations
    ////////////////////////////////////////////////////////

    /// set src as a link target of dst
    fn symlink(
        &mut self,
        src: &str,
        dst: &str,
        actionexec: Option<ActionExec>,
        absolute: bool,
    ) -> Ret {
        let mut overwrite = !self.safe;
        if lexists(dst) {
            if utils::realpath(dst) == utils::realpath(src) {
                dbg_log!("ignoring \"{dst}\", link already exists");
                return (false, None);
            }
            if self.dry {
                log::dry(&format!("would remove {dst} and link to {src}"));
                return (true, None);
            }
            if self.showdiff {
                self.show_diff_before_write(src, dst, None);
            }
            let msg = format!("Remove \"{dst}\" for link creation?");
            if self.safe && !log::ask(&msg) {
                return (false, Some("aborted".into()));
            }
            if self.backup && !is_dir(dst) && !self.backup_file(dst) {
                return (false, Some(format!("could not backup {dst}")));
            }
            overwrite = true;
            if let Err(e) = utils::try_removepath(dst) {
                return (false, Some(format!("something went wrong with {src}: {e}")));
            }
        }
        if self.dry {
            log::dry(&format!("would link {dst} to {src}"));
            return (true, None);
        }
        let base = utils::dirname(dst);
        if !self.create_dirs(&base) {
            return (false, Some(format!("error creating directory for {dst}")));
        }
        let (ok, err) = self.exec_pre_actions(actionexec);
        if !ok {
            return (false, err);
        }
        // re-check in case action created the file
        if lexists(dst) {
            let msg = format!("Remove \"{dst}\" for link creation?");
            if self.safe && !overwrite && !log::ask(&msg) {
                return (false, Some("aborted".into()));
            }
            if let Err(e) = utils::try_removepath(dst) {
                return (false, Some(format!("something went wrong with {src}: {e}")));
            }
        }
        let lnk_src = if absolute {
            src.to_string()
        } else {
            let dstrel = if is_dir(dst) {
                dst.to_string()
            } else {
                utils::dirname(dst)
            };
            utils::relpath(src, &dstrel)
        };
        if let Err(e) = std::os::unix::fs::symlink(&lnk_src, dst) {
            return (false, Some(format!("something went wrong with {src}: {e}")));
        }
        dbg_log!("symlink {dst} to {lnk_src} (mode:{:o})", get_file_perm(dst));
        if !self.comparing {
            log::sub(&format!("linked {dst} to {lnk_src}"));
        }
        (true, None)
    }

    /// install src to dst when src is a file
    fn copy_file(
        &mut self,
        templater: &mut Templategen,
        src: &str,
        dst: &str,
        actionexec: Option<ActionExec>,
        spec: Spec,
    ) -> Ret {
        dbg_log!(
            "deploy file: {src} (template: {}, noempty: {})",
            spec.is_template,
            spec.noempty
        );
        if must_ignore(&[src, dst], spec.ignore, false) {
            dbg_log!("ignoring install of {src} to {dst}");
            return (false, None);
        }
        if samefile(src, dst) {
            return (false, Some(format!("dotfile points to itself: {dst}")));
        }
        if !utils::exists(src) {
            return (false, Some(format!("source dotfile does not exist: {src}")));
        }
        let mut content: Option<Vec<u8>> = None;
        if spec.is_template {
            dbg_log!("it is a template: {src}");
            let mut tmpvars = Vars::new();
            tmpvars.insert(
                "_dotfile_sub_abs_src".into(),
                Value::String(src.to_string()),
            );
            tmpvars.insert(
                "_dotfile_sub_abs_dst".into(),
                Value::String(dst.to_string()),
            );
            let saved = templater.add_tmp_vars(Some(&tmpvars));
            let res = templater.generate(src);
            templater.restore_vars(&saved);
            match res {
                Ok(c) => {
                    if spec.noempty && content_empty(&c) {
                        dbg_log!("ignoring empty template: {src}");
                        return (false, None);
                    }
                    content = Some(c);
                }
                Err(e) => return (false, Some(e.to_string())),
            }
        }
        let (ret, err) = self.write(src, dst, content.as_deref(), actionexec);
        if ret && err.is_none() {
            dbg_log!("installed file {src} to {dst} ({:o})", get_file_perm(src));
            if !self.dry && !self.comparing {
                log::sub(&format!("install {src} to {dst}"));
            }
        }
        (ret, err)
    }

    /// install src to dst when src is a directory; third value is the list of
    /// managed dotfiles in the destination
    fn copy_dir(
        &mut self,
        templater: &mut Templategen,
        src: &str,
        dst: &str,
        actionexec: Option<ActionExec>,
        spec: Spec,
        dir_as_block: bool,
    ) -> (bool, Option<String>, Vec<String>) {
        dbg_log!("deploy dir {src} (dir_as_block: {dir_as_block})");

        if dir_as_block {
            if utils::exists(dst) {
                let msg = format!("Overwrite entire directory \"{dst}\" with \"{src}\"?");
                if self.safe && !log::ask(&msg) {
                    return (false, Some("aborted".into()), Vec::new());
                }
                if self.dry {
                    log::dry(&format!("would rm -r {dst}"));
                } else {
                    dbg_log!("rm -r {dst}");
                    if !removepath(dst) {
                        let msg = format!("unable to remove {dst}, do manually");
                        log::warn(&msg);
                        return (false, Some(msg), Vec::new());
                    }
                }
            }
            let parent = utils::dirname(dst);
            if !utils::exists(&parent) {
                if self.dry {
                    log::dry(&format!("would mkdir -p {parent}"));
                } else if !self.create_dirs(&parent) {
                    return (
                        false,
                        Some(format!("error creating directory for {dst}")),
                        Vec::new(),
                    );
                }
            }
            if self.dry {
                log::dry(&format!("would cp -r {src} {dst}"));
                return (true, None, vec![dst.to_string()]);
            }
            let (ok, err) = self.exec_pre_actions(actionexec);
            if !ok {
                return (false, err, Vec::new());
            }
            if let Err(e) = copytree(src, dst) {
                let err = format!("{src} installation failed: {e}");
                log::warn(&err);
                return (false, Some(err), Vec::new());
            }
            let mut installed = Vec::new();
            for (root, _, files) in utils::walk(dst, false) {
                for f in files {
                    installed.push(join(&root, &f));
                }
            }
            if !self.comparing {
                log::sub(&format!("installed directory {src} to {dst} as a block"));
            }
            return (true, None, installed);
        }

        let mut ret = false;
        let mut dst_dotfiles = Vec::new();
        for entry in utils::listdir(src) {
            let fpath = join(src, &entry);
            dbg_log!("deploy sub from {dst}: {entry}");
            if !is_dir(&fpath) {
                let fdst = join(dst, &entry);
                dst_dotfiles.push(fdst.clone());
                let (res, err) = self.copy_file(templater, &fpath, &fdst, actionexec, spec);
                if !res && err.is_some() {
                    return (res, err, Vec::new());
                }
                self.apply_chmod_after_install(
                    &fpath,
                    &fdst,
                    ret,
                    &err,
                    spec.chmod,
                    true,
                    LinkType::NoLink,
                );
                if res {
                    ret = true;
                }
            } else {
                let dpath = join(dst, &entry);
                dst_dotfiles.push(dpath.clone());
                let sub_block = self.must_treat_dir_as_block(&fpath, spec.dir_as_block);
                let (res, err, subs) =
                    self.copy_dir(templater, &fpath, &dpath, actionexec, spec, sub_block);
                dst_dotfiles.extend(subs);
                if !res && err.is_some() {
                    return (res, err, Vec::new());
                }
                if res {
                    ret = true;
                }
            }
        }
        (ret, None, dst_dotfiles)
    }

    fn is_path_in(path: &str, paths: &[String]) -> bool {
        paths.iter().any(|p| samefile(path, p))
    }

    /// with --remove-existing, remove any file in managed directory not handled by dotdrop
    fn remove_existing_in_dir(&self, directory: &str, installed_files: &[String]) {
        if installed_files.is_empty() || !is_dir(directory) {
            return;
        }
        let mut to_remove = Vec::new();
        for (root, dirs, files) in utils::walk(directory, false) {
            for name in files.iter().chain(dirs.iter()) {
                let path = join(&root, name);
                if Self::is_path_in(&path, installed_files) {
                    continue;
                }
                to_remove.push(utils::abspath(&path));
            }
        }
        for path in to_remove {
            if self.dry {
                log::dry(&format!("would remove {path}"));
                continue;
            }
            if self.safe && !log::ask(&format!("remove unmanaged \"{path}\"")) {
                return;
            }
            dbg_log!("removing unmanaged file \"{path}\"");
            if !removepath(&path) {
                log::warn(format!("unable to remove {path}"));
            }
        }
    }

    /// remove stale links previously installed by link_children
    fn remove_stale_link_children(&self, source: &str, destination: &str) {
        if !is_dir(destination) {
            return;
        }
        let managed_roots = [utils::realpath(source), utils::realpath(&self.workdir)];
        for child in utils::listdir(destination) {
            if lexists(&join(source, &child)) {
                continue;
            }
            let path = join(destination, &child);
            if !Self::is_managed_dangling_link(&path, &managed_roots) {
                continue;
            }
            if self.dry {
                log::dry(&format!("would remove stale link \"{path}\""));
                continue;
            }
            if self.safe && !log::ask(&format!("remove stale link \"{path}\"")) {
                return;
            }
            if !removepath(&path) {
                log::warn(format!("unable to remove {path}"));
                continue;
            }
            log::sub(&format!("removed stale link \"{path}\""));
        }
    }

    fn is_managed_dangling_link(path: &str, managed_roots: &[String]) -> bool {
        if !utils::is_link(path) || utils::exists(path) {
            return false;
        }
        let Ok(link) = fs::read_link(path) else {
            return false;
        };
        let target = utils::realpath(&join(&utils::dirname(path), &utils::os(link.as_os_str())));
        managed_roots.iter().any(|root| {
            target == *root || target.starts_with(&format!("{}/", root.trim_end_matches('/')))
        })
    }

    /// write the content (or copy the file when there is none) to dst
    fn write_content_to_file(content: Option<&[u8]>, src: &str, dst: &str) -> Ret {
        match content.filter(|c| !c.is_empty()) {
            Some(c) => match fs::File::create(dst).and_then(|mut f| f.write_all(c)) {
                Ok(()) => (true, None),
                Err(e) if e.raw_os_error() == Some(20) => {
                    (false, Some(format!("opening dest file: {e}")))
                }
                Err(e) => (false, Some(e.to_string())),
            },
            None => {
                // do NOT copy meta here
                let res = fs::File::open(src).and_then(|mut s| {
                    fs::File::create(dst).and_then(|mut d| std::io::copy(&mut s, &mut d))
                });
                match res {
                    Ok(_) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                }
            }
        }
    }

    /// copy dotfile / write content to file
    fn write(
        &mut self,
        src: &str,
        dst: &str,
        content: Option<&[u8]>,
        actionexec: Option<ActionExec>,
    ) -> Ret {
        let mut overwrite = !self.safe;
        if self.dry {
            log::dry(&format!("would install {dst}"));
            return (true, None);
        }
        if lexists(dst) {
            dbg_log!("file already exists on filesystem: {dst}");
            if fs::metadata(dst).is_err() {
                // broken symlink
                return (false, Some(format!("broken symlink {dst}")));
            }
            if self.diff && !self.is_different(src, dst, content) {
                dbg_log!("{dst} is the same");
                return (false, None);
            }
            if self.safe {
                dbg_log!("change detected for {dst}");
                if self.showdiff {
                    self.show_diff_before_write(src, dst, content);
                }
                if !log::ask(&format!("Overwrite \"{dst}\"")) {
                    return (false, Some("aborted".into()));
                }
                overwrite = true;
            }
            if self.backup && !self.backup_file(dst) {
                return (false, Some(format!("could not backup {dst}")));
            }
        } else {
            dbg_log!("file does not exist on filesystem: {dst}");
        }

        let base = utils::dirname(dst);
        if !self.create_dirs(&base) {
            return (false, Some(format!("creating directory for {dst}")));
        }
        let (ok, err) = self.exec_pre_actions(actionexec);
        if !ok {
            return (false, err);
        }
        dbg_log!("installing file to \"{dst}\"");
        // re-check in case action created the file
        if self.safe && !overwrite && lexists(dst) && !log::ask(&format!("Overwrite \"{dst}\"")) {
            log::warn(format!("ignoring {dst}"));
            return (false, Some("aborted".into()));
        }
        Self::write_content_to_file(content, src, dst)
    }

    ////////////////////////////////////////////////////////
    // helpers
    ////////////////////////////////////////////////////////

    /// True if the file is different and needs to be installed
    fn is_different(&self, src: &str, dst: &str, content: Option<&[u8]>) -> bool {
        let mut tmp = None;
        let mut src = src.to_string();
        if let Some(c) = content.filter(|c| !c.is_empty()) {
            if let Ok(t) = write_to_tmpfile(c) {
                src = t.clone();
                tmp = Some(t);
            }
        }
        let ret = !utils::same_content(&src, dst);
        if ret {
            dbg_log!("content differ");
        }
        if let Some(t) = tmp {
            removepath(&t);
        }
        ret
    }

    fn show_diff_before_write(&self, src: &str, dst: &str, content: Option<&[u8]>) -> String {
        let mut tmp = None;
        let mut src = src.to_string();
        if let Some(c) = content.filter(|c| !c.is_empty()) {
            if let Ok(t) = write_to_tmpfile(c) {
                src = t.clone();
                tmp = Some(t);
            }
        }
        let diff = utils::diff(dst, &src, &self.diff_cmd);
        if let Some(t) = tmp {
            removepath(&t);
        }
        if !diff.is_empty() {
            log::log(&format!("diff \"{dst}\" VS \"{src}\""));
            log::emph(&diff);
        }
        diff
    }

    /// mkdir -p
    fn create_dirs(&self, directory: &str) -> bool {
        if !self.create && !utils::exists(directory) {
            dbg_log!("no mkdir as \"create\" set to false in config");
            return false;
        }
        if utils::exists(directory) {
            return true;
        }
        if self.dry {
            log::dry(&format!("would mkdir -p {directory}"));
            return true;
        }
        dbg_log!("mkdir -p {directory}");
        let _ = fs::create_dir_all(directory);
        utils::exists(directory)
    }

    /// backup file pointed by path
    fn backup_file(&self, path: &str) -> bool {
        if self.dry {
            return true;
        }
        let dst = format!("{}{}", path.trim_end_matches('/'), self.backup_suffix);
        log::log(&format!("backup {path} to {dst}"));
        // copy to preserve mode on chmod=preserve
        if !copyfile(path, &dst) || !utils::exists(&dst) {
            return false;
        }
        if let Ok(meta) = fs::metadata(path) {
            use std::os::unix::fs::MetadataExt;
            let _ = std::os::unix::fs::chown(&dst, Some(meta.uid()), Some(meta.gid()));
        }
        true
    }

    fn exec_pre_actions(&mut self, actionexec: Option<ActionExec>) -> Ret {
        if self.action_executed {
            return (true, None);
        }
        let Some(exec) = actionexec else {
            return (true, None);
        };
        let ret = exec();
        self.action_executed = true;
        ret
    }

    fn log_install(&self, ok: bool, err: Option<String>) -> Ret {
        if ok {
            dbg_log!("install: SUCCESS");
        } else if let Some(e) = &err {
            dbg_log!("install: ERROR: {e}");
        } else {
            dbg_log!("install: IGNORED");
        }
        (ok, err)
    }

    /// check and normalize params: returns (src, dst)
    fn check_paths(&self, src: &str, dst: &str) -> Option<(String, String)> {
        if dst.is_empty() || src.is_empty() {
            dbg_log!("empty dst or src for {src}");
            return None;
        }
        Some((
            utils::normpath(&utils::expanduser(src)),
            utils::normpath(&utils::expanduser(dst)),
        ))
    }
}
