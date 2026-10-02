//! The dotdrop commands.

use crate::cfg_aggregator::CfgAggregator;
use crate::comparator::Comparator;
use crate::dbg_log;
use crate::error::Result;
use crate::importer::{Imported, Importer};
use crate::installer::{ActionExec, Installer, InstallerOpts, Ret, Spec};
use crate::linktype::LinkType;
use crate::log;
use crate::model::{ACTION_POST, ACTION_PRE, Action, Chmod, Dotfile};
use crate::options::{BACKUP_SUFFIX, Options};
use crate::templategen::Templategen;
use crate::uninstaller::Uninstaller;
use crate::updater::Updater;
use crate::utils::{
    self, dir_empty, get_tmpdir, ignores_to_absolute, join, pivot_path, removepath, uniq_list,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const TRANS_SUFFIX: &str = "trans";

/// execute actions; returns (ok, error)
fn action_executor(
    opts: &Options,
    actions: &[Action],
    defactions: &[Action],
    templater: &Templategen,
    post: bool,
) -> Ret {
    let actiontype = if post { "post" } else { "pre" };
    for action in defactions {
        if opts.dry {
            log::dry(&format!("would execute def-{actiontype}-action: {action}"));
            continue;
        }
        dbg_log!("executing def-{actiontype}-action: {action}");
        if !action.execute(Some(templater)) {
            let err = format!("def-{actiontype}-action \"{}\" failed", action.key);
            log::err(&err);
            return (false, Some(err));
        }
    }
    for action in actions {
        if opts.dry {
            log::dry(&format!("would execute {actiontype}-action: {action}"));
            continue;
        }
        dbg_log!("executing {actiontype}-action: {action}");
        if !action.execute(Some(templater)) {
            let err = format!("{actiontype}-action \"{}\" failed", action.key);
            log::err(&err);
            return (false, Some(err));
        }
    }
    (true, None)
}

fn get_templater(opts: &Options) -> Templategen {
    Templategen::new(
        &opts.settings.dotpath,
        Some(&opts.variables),
        &opts.settings.func_file,
        &opts.settings.filter_file,
    )
}

fn get_installer(opts: &Options, tmpdir: Option<String>) -> Installer {
    Installer::new(InstallerOpts {
        base: opts.settings.dotpath.clone(),
        create: opts.settings.create,
        backup: opts.settings.backup,
        dry: opts.dry,
        safe: opts.safe,
        workdir: opts.settings.workdir.clone(),
        diff: opts.install_diff,
        totemp: tmpdir,
        showdiff: opts.install_showdiff,
        backup_suffix: BACKUP_SUFFIX.to_string(),
        diff_cmd: opts.settings.diff_command.clone(),
        remove_existing_in_dir: opts.install_remove_existing,
        force_chmod: opts.settings.force_chmod,
    })
}

/// apply the install transformation; the new source or None on failure
fn apply_install_trans(
    dotpath: &str,
    dotfile: &Dotfile,
    templater: &Templategen,
) -> Option<String> {
    let trans = dotfile.trans_install.as_ref()?;
    let new_src = format!("{}.{TRANS_SUFFIX}", dotfile.src);
    dbg_log!("executing install transformation: {trans}");
    let srcpath = join(dotpath, &dotfile.src);
    let temp = join(dotpath, &new_src);
    if !trans.transform(&srcpath, &temp, Some(templater)) {
        log::err(format!(
            "install transformation \"{}\"failed for {}",
            trans.key, dotfile.key
        ));
        if utils::exists(&new_src) {
            removepath(&new_src);
        }
        return None;
    }
    Some(new_src)
}

fn is_template(dotfile: &Dotfile, src: &str) -> bool {
    dotfile.template && Templategen::path_is_template(src)
}

////////////////////////////////////////////////////////////
// install
////////////////////////////////////////////////////////////

/// install a dotfile: (success, dotfile key, error)
fn dotfile_install(
    opts: &Options,
    dotfile: &Dotfile,
    tmpdir: Option<&str>,
) -> (bool, String, Option<String>) {
    let mut inst = get_installer(opts, tmpdir.map(String::from));
    let mut templ = get_templater(opts);
    templ.add_tmp_vars(Some(&dotfile.dotfile_variables()));
    // the actions use their own templater (the installer owns the other one)
    let mut act_templ = get_templater(opts);
    act_templ.add_tmp_vars(Some(&dotfile.dotfile_variables()));

    let preactions = if opts.install_temporary {
        Vec::new()
    } else {
        dotfile.pre_actions()
    };
    let defpre = opts.install_default_actions(ACTION_PRE);
    let pre_exec = || action_executor(opts, &preactions, &defpre, &act_templ, false);

    dbg_log!("installing dotfile: \"{}\"", dotfile.key);
    dbg_log!("{}", dotfile.prt());

    let mut ignores = opts.settings.instignore.clone();
    ignores.extend(
        dotfile
            .instignore
            .iter()
            .filter(|i| !ignores.contains(i))
            .cloned()
            .collect::<Vec<_>>(),
    );
    let ignores = ignores_to_absolute(&ignores, &[&dotfile.dst, &dotfile.src]);

    let exec: ActionExec = &pre_exec;
    let (ret, err);
    if dotfile.link != LinkType::NoLink {
        let spec = Spec {
            noempty: false,
            ignore: &ignores,
            is_template: is_template(dotfile, &dotfile.src),
            chmod: dotfile.chmod,
            dir_as_block: &dotfile.dir_as_block,
        };
        (ret, err) = inst.install(
            &mut templ,
            &dotfile.src,
            &dotfile.dst,
            dotfile.link,
            Some(exec),
            spec,
        );
    } else {
        let mut src = dotfile.src.clone();
        let mut tmp = None;
        if dotfile.trans_install.is_some() {
            tmp = apply_install_trans(&opts.settings.dotpath, dotfile, &templ);
            match &tmp {
                Some(t) => src = t.clone(),
                None => return (false, dotfile.key.clone(), None),
            }
        }
        let spec = Spec {
            noempty: dotfile.noempty,
            ignore: &ignores,
            is_template: is_template(dotfile, &src),
            chmod: dotfile.chmod,
            dir_as_block: &dotfile.dir_as_block,
        };
        (ret, err) = inst.install(
            &mut templ,
            &src,
            &dotfile.dst,
            LinkType::NoLink,
            Some(exec),
            spec,
        );
        if let Some(t) = tmp {
            let t = join(&opts.settings.dotpath, &t);
            if utils::exists(&t) {
                removepath(&t);
            }
        }
    }

    let run_post = || {
        let defpost = opts.install_default_actions(ACTION_POST);
        let postactions = dotfile.post_actions();
        action_executor(opts, &postactions, &defpost, &act_templ, true);
    };
    if ret {
        if !opts.install_temporary {
            run_post();
        }
    } else if opts.install_force_action {
        dbg_log!("force pre action execution ...");
        pre_exec();
        dbg_log!("force post action execution ...");
        run_post();
    }
    (ret, dotfile.key.clone(), err)
}

/// run `job` over `items` with `workers` threads; results keep no order
fn parallel<T: Sync, R: Send>(items: &[T], workers: usize, job: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if workers <= 1 {
        return items.iter().map(&job).collect();
    }
    dbg_log!("run with {workers} workers");
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(i) else { break };
                    let r = job(item);
                    results.lock().unwrap_or_else(|e| e.into_inner()).push(r);
                }
            });
        }
    });
    results.into_inner().unwrap_or_else(|e| e.into_inner())
}

fn adapt_workers(opts: &Options) -> usize {
    let mut workers = opts.workers;
    if opts.safe && workers > 1 {
        log::warn("workers set to 1 when --force is not used");
        workers = 1;
    }
    if opts.dry && workers > 1 {
        log::warn("workers set to 1 when --dry is used");
        workers = 1;
    }
    workers
}

pub fn cmd_install(opts: &Options, conf: &CfgAggregator) -> bool {
    let mut dotfiles = opts.dotfiles.clone();
    let prof = conf.get_profile(None);

    // hidden profiles cannot be directly installed
    if prof.is_some_and(|p| p.hidden()) && opts.safe {
        log::err(format!(
            "profile \"{}\" is hidden, use --force",
            opts.profile
        ));
        return false;
    }
    let workers = adapt_workers(opts);
    let pro_pre = prof.map(|p| p.pre_actions()).unwrap_or_default();
    let pro_post = prof.map(|p| p.post_actions()).unwrap_or_default();

    if !opts.install_keys.is_empty() {
        let uniq = uniq_list(&opts.install_keys);
        dotfiles.retain(|d| uniq.contains(&d.key));
    }
    if dotfiles.is_empty() {
        log::warn(format!(
            "no dotfile to install for this profile (\"{}\")",
            opts.profile
        ));
        return false;
    }
    dbg_log!(
        "dotfiles registered for install: {:?}",
        dotfiles.iter().map(|d| &d.key).collect::<Vec<_>>()
    );

    let tmpdir = opts.install_temporary.then(get_tmpdir);

    // clear the workdir
    if opts.install_clear_workdir && !opts.dry {
        dbg_log!("clearing the workdir under {}", opts.settings.workdir);
        for (root, _, files) in utils::walk(&opts.settings.workdir, false) {
            for f in files {
                removepath(&join(&root, &f));
            }
        }
    }

    // execute profile pre-action
    dbg_log!("run {} profile pre actions", pro_pre.len());
    let templ = get_templater(opts);
    if !action_executor(opts, &pro_pre, &[], &templ, false).0 {
        return false;
    }

    let results = parallel(&dotfiles, workers, |d| {
        dotfile_install(opts, d, tmpdir.as_deref())
    });
    let mut installed = Vec::new();
    for (ok, key, err) in results {
        if ok {
            installed.push(key);
        } else if let Some(e) = err {
            log::err(format!("installing \"{key}\" failed: {e}"));
        }
    }

    // execute profile post-action
    if !installed.is_empty() || opts.install_force_action {
        dbg_log!("run {} profile post actions", pro_post.len());
        if !action_executor(opts, &pro_post, &[], &templ, false).0 {
            return false;
        }
    }
    dbg_log!("install done: installed \"{}\"", installed.join(","));
    if let Some(t) = &tmpdir {
        log::log(&format!("\ninstalled to tmp \"{t}\"."));
    }
    log::log(&format!("\n{} dotfile(s) installed.", installed.len()));
    true
}

////////////////////////////////////////////////////////////
// compare
////////////////////////////////////////////////////////////

fn dotfile_compare(opts: &Options, dotfile: &Dotfile, tmp: &str) -> bool {
    let mut templ = get_templater(opts);
    let ignore_missing = opts.ignore_missing_in_dotdrop || dotfile.ignore_missing_in_dotdrop;
    let mut inst = Installer::new(InstallerOpts {
        force_chmod: true,
        ..installer_opts_for_compare(opts)
    });
    let comp = Comparator::new(&opts.settings.diff_command, ignore_missing);
    templ.add_tmp_vars(Some(&dotfile.dotfile_variables()));

    dbg_log!("comparing {dotfile}");
    let mut src = dotfile.src.clone();
    if !utils::lexists(&utils::expanduser(&dotfile.dst)) {
        log::log(&format!(
            "=> compare {}: \"{}\" does not exist on destination",
            dotfile.key, dotfile.dst
        ));
        return false;
    }

    // apply transformation
    let mut tmpsrc = None;
    if dotfile.trans_install.is_some() {
        dbg_log!("applying transformation before comparing");
        match apply_install_trans(&opts.settings.dotpath, dotfile, &templ) {
            Some(t) => {
                src = t.clone();
                tmpsrc = Some(t);
            }
            None => return false,
        }
    }

    // is a symlink pointing to itself
    let asrc = join(&opts.settings.dotpath, &utils::expanduser(&src));
    let adst = utils::expanduser(&dotfile.dst);
    if utils::samefile(&asrc, &adst) {
        dbg_log!(
            "=> compare {}: diffing with \"{}\"",
            dotfile.key,
            dotfile.dst
        );
        dbg_log!("points to itself");
        return true;
    }

    let mut ignores = opts.compare_ignore.clone();
    ignores.extend(
        dotfile
            .cmpignore
            .iter()
            .filter(|i| !ignores.contains(i))
            .cloned()
            .collect::<Vec<_>>(),
    );
    let ignores = ignores_to_absolute(&ignores, &[&dotfile.dst, &dotfile.src]);

    let mut insttmp = None;
    if is_template(dotfile, &src) {
        // install dotfile to temporary dir for compare
        let spec = Spec {
            noempty: false,
            ignore: &[],
            is_template: true,
            chmod: dotfile.chmod,
            dir_as_block: &[],
        };
        let (ret, err, path) =
            inst.install_to_temp(&mut templ, tmp, &src, &dotfile.dst, spec, true);
        if !ret {
            let err = err.unwrap_or_default();
            log::log(&format!("=> compare {} error: {err}", dotfile.key));
            log::err(&err);
            return false;
        }
        src = path.clone().unwrap_or(src);
        insttmp = path;
    }

    // compare (needs to be executed before cleaning)
    let full_src = join(&opts.settings.dotpath, &src);
    let diff = comp.compare(&full_src, &dotfile.dst, &ignores, dotfile.chmod);

    if let Some(t) = tmpsrc {
        let t = join(&opts.settings.dotpath, &t);
        if utils::exists(&t) {
            removepath(&t);
        }
    }
    if let Some(t) = insttmp {
        if utils::exists(&t) {
            removepath(&t);
        }
    }

    if !diff.is_empty() {
        if opts.compare_fileonly {
            log::log(&format!(
                "=> differ: \"{}\" \"{}\"",
                dotfile.key, dotfile.dst
            ));
        } else {
            log::log(&format!(
                "=> compare {}: diffing with \"{}\"",
                dotfile.key, dotfile.dst
            ));
            log::emph(&diff);
        }
        return false;
    }
    dbg_log!(
        "=> compare {}: diffing with \"{}\"",
        dotfile.key,
        dotfile.dst
    );
    dbg_log!("same file");
    true
}

fn installer_opts_for_compare(opts: &Options) -> InstallerOpts {
    InstallerOpts {
        base: opts.settings.dotpath.clone(),
        create: opts.settings.create,
        backup: opts.settings.backup,
        dry: opts.dry,
        safe: false,
        workdir: opts.settings.workdir.clone(),
        diff: true,
        totemp: None,
        showdiff: false,
        backup_suffix: BACKUP_SUFFIX.to_string(),
        diff_cmd: opts.settings.diff_command.clone(),
        remove_existing_in_dir: false,
        force_chmod: true,
    }
}

/// list the files of the workdir that do not exist in dotdrop
fn workdir_enum(opts: &Options) -> usize {
    let workdir = &opts.settings.workdir;
    let mut workdir_files: Vec<String> = Vec::new();
    for (root, _, files) in utils::walk(workdir, false) {
        for f in files {
            workdir_files.push(join(&root, &f));
        }
    }
    for dotfile in &opts.dotfiles {
        let src = join(&opts.settings.dotpath, &dotfile.src);
        if dotfile.link == LinkType::NoLink || !Templategen::path_is_template(&src) {
            continue;
        }
        let newpath = pivot_path(&dotfile.dst, workdir, true);
        if utils::is_dir(&newpath) {
            let pattern = format!("{newpath}/*");
            workdir_files.retain(|f| !utils::fnmatch(f, &pattern));
            for child in utils::listdir(&newpath) {
                let c = join(&newpath, &child);
                workdir_files.retain(|f| *f != c);
            }
        } else {
            workdir_files.retain(|f| *f != newpath);
        }
    }
    for w in &workdir_files {
        log::log(&format!("=> \"{w}\" does not exist in dotdrop"));
    }
    workdir_files.len()
}

fn select(
    selections: &[String],
    dotfiles: &[std::sync::Arc<Dotfile>],
) -> Vec<std::sync::Arc<Dotfile>> {
    let mut selected = Vec::new();
    for sel in selections {
        let want = utils::expanduser(sel);
        match dotfiles.iter().find(|d| utils::expanduser(&d.dst) == want) {
            Some(d) => selected.push(d.clone()),
            None => log::err(format!("no dotfile matches \"{sel}\"")),
        }
    }
    selected
}

/// compare dotfiles and return true if all identical
pub fn cmd_compare(opts: &Options, tmp: &str) -> bool {
    if opts.dotfiles.is_empty() {
        log::warn(format!(
            "no dotfile defined for this profile (\"{}\")",
            opts.profile
        ));
        return true;
    }
    let selected = if opts.compare_focus.is_empty() {
        opts.dotfiles.clone()
    } else {
        select(&opts.compare_focus, &opts.dotfiles)
    };
    if selected.is_empty() {
        log::log("\nno dotfile to compare");
        return false;
    }
    let real: Vec<_> = selected
        .into_iter()
        .filter(|d| !(d.src.is_empty() && d.dst.is_empty()))
        .collect();
    let results = parallel(&real, opts.workers, |d| dotfile_compare(opts, d, tmp));
    let mut same = results.iter().all(|r| *r);
    if opts.settings.compare_workdir && workdir_enum(opts) > 0 {
        same = false;
    }
    log::log(&format!("\n{} dotfile(s) compared.", results.len()));
    same
}

////////////////////////////////////////////////////////////
// update / import
////////////////////////////////////////////////////////////

pub fn cmd_update(opts: &Options, conf: &mut CfgAggregator) -> bool {
    let mut cnt = 0;
    let mut paths = opts.update_path.clone();
    let iskey = opts.update_iskey;

    if !conf.profiles.iter().any(|p| p.key == opts.profile) {
        log::err(format!("no such profile \"{}\"", opts.profile));
        return false;
    }
    if paths.is_empty() {
        paths = if iskey {
            opts.dotfiles.iter().map(|d| d.key.clone()).collect()
        } else {
            opts.dotfiles.iter().map(|d| d.dst.clone()).collect()
        };
        let msg = format!("Update all dotfiles for profile \"{}\"", opts.profile);
        if opts.safe && !log::ask(&msg) {
            log::log(&format!("\n{cnt} file(s) updated."));
            return false;
        }
    }
    if paths.is_empty() {
        log::log("\nno dotfile to update");
        return true;
    }
    dbg_log!("dotfile to update: {paths:?}");

    // updates change the config, they are always sequential
    let mut updater = Updater::new(
        &opts.settings.dotpath,
        &opts.variables,
        &opts.profile,
        opts.dry,
        opts.safe,
        opts.update_ignore.clone(),
        opts.update_showpatch,
        opts.ignore_missing_in_dotdrop,
    );
    for path in &paths {
        let ok = if iskey {
            updater.update_key(conf, path)
        } else {
            updater.update_path(conf, path)
        };
        if ok {
            cnt += 1;
        }
    }
    log::log(&format!("\n{cnt} file(s) updated."));
    cnt == paths.len()
}

pub fn cmd_import(opts: &Options, conf: &mut CfgAggregator) -> Result<bool> {
    let mut ret = true;
    let mut cnt = 0;
    let mut importer = Importer::new(
        &opts.profile,
        &opts.settings.dotpath,
        &opts.settings.diff_command,
        &opts.variables,
        opts.dry,
        opts.safe,
        opts.settings.keepdot,
        &opts.import_ignore,
        opts.import_force_key.clone(),
    )?;
    for path in &opts.import_path {
        match importer.import_path(
            conf,
            path,
            opts.import_as.as_deref(),
            opts.import_link,
            opts.import_mode,
            opts.import_trans_install.as_deref(),
            opts.import_trans_update.as_deref(),
        ) {
            Imported::Failed => ret = false,
            Imported::One => cnt += 1,
            Imported::Ignored => {}
        }
    }
    if opts.dry {
        log::dry("new config file would be:");
        log::raw(&conf.dump()?);
    } else {
        conf.save()?;
    }
    log::log(&format!("\n{cnt} file(s) imported."));
    Ok(ret)
}

////////////////////////////////////////////////////////////
// listings
////////////////////////////////////////////////////////////

fn profile_line(dotfiles: usize, description: &Option<String>) -> String {
    let mut line = format!(" ({dotfiles} dotfiles)");
    if let Some(d) = description {
        line += &format!(" - {d}");
    }
    line
}

pub fn cmd_list_profiles(opts: &Options, conf: &CfgAggregator) {
    // hidden profiles are not displayed
    let profiles: Vec<_> = conf.profiles.iter().filter(|p| !p.hidden()).collect();
    log::emph("Available profile(s):\n\n");
    if opts.profiles_grepable {
        for p in profiles {
            log::raw(&p.key);
        }
    } else {
        // profiles without a group first, in config order
        for p in profiles.iter().filter(|p| p.group.is_none()) {
            log::sub_noend(&p.key);
            log::log(&profile_line(p.dotfiles.len(), &p.description));
        }
        // then the remaining profiles grouped by group, in order of appearance
        let mut groups: Vec<&String> = Vec::new();
        for p in &profiles {
            if let Some(g) = &p.group {
                if !groups.contains(&g) {
                    groups.push(g);
                }
            }
        }
        for g in groups {
            log::log(&format!("group \"{g}\":"));
            for p in profiles.iter().filter(|p| p.group.as_ref() == Some(g)) {
                log::sub_noend(&p.key);
                log::log(&profile_line(p.dotfiles.len(), &p.description));
            }
        }
    }
    log::log("");
}

fn chmod_str(c: &Option<Chmod>) -> String {
    c.map(|c| c.to_string())
        .unwrap_or_else(|| "None".to_string())
}

pub fn cmd_files(opts: &Options, conf: &CfgAggregator) {
    if !conf.profiles.iter().any(|p| p.key == opts.profile) {
        log::warn(format!("unknown profile \"{}\"", opts.profile));
        return;
    }
    let what = if opts.files_templateonly {
        "Template(s)"
    } else {
        "Dotfile(s)"
    };
    log::emph(&format!("{what} for profile \"{}\":\n\n", opts.profile));
    for dotfile in &opts.dotfiles {
        if opts.files_templateonly
            && !Templategen::path_is_template(&join(&opts.settings.dotpath, &dotfile.src))
        {
            continue;
        }
        if opts.files_grepable {
            log::raw(&format!(
                "{},dst:{},src:{},link:{},chmod:{}",
                dotfile.key,
                dotfile.dst,
                dotfile.src,
                dotfile.link,
                chmod_str(&dotfile.chmod)
            ));
        } else {
            log::log_bold(&dotfile.key);
            log::sub(&format!("dst: {}", dotfile.dst));
            log::sub(&format!("src: {}", dotfile.src));
            log::sub(&format!("link: {}", dotfile.link));
            if let Some(c) = &dotfile.chmod {
                log::sub(&format!("chmod: {c}"));
            }
        }
    }
    log::log("");
}

/// display details on all files under a dotfile entry
fn detail(dotpath: &str, dotfile: &Dotfile) {
    let chmod = match &dotfile.chmod {
        Some(Chmod::Mode(m)) => m.to_string(),
        Some(Chmod::Preserve) => "preserve".to_string(),
        None => "None".to_string(),
    };
    log::log(&format!(
        "{} (dst: \"{}\", link: \"{}\", chmod: \"{chmod}\")",
        dotfile.key, dotfile.dst, dotfile.link
    ));
    let path = join(dotpath, &utils::expanduser(&dotfile.src));
    let tpl = |p: &str| {
        if dotfile.template && Templategen::path_is_template(p) {
            "yes"
        } else {
            "no"
        }
    };
    if !utils::is_dir(&path) {
        log::sub(&format!("{path} (template:{})", tpl(&path)));
    } else {
        for (root, _, files) in utils::walk(&path, false) {
            for f in files {
                let fpath = join(&root, &f);
                log::sub(&format!("{fpath} (template:{})", tpl(&fpath)));
            }
        }
    }
}

pub fn cmd_detail(opts: &Options, conf: &CfgAggregator) {
    if !conf.profiles.iter().any(|p| p.key == opts.profile) {
        log::warn(format!("unknown profile \"{}\"", opts.profile));
        return;
    }
    let mut dotfiles = opts.dotfiles.clone();
    if !opts.detail_keys.is_empty() {
        let uniq = uniq_list(&opts.detail_keys);
        dotfiles.retain(|d| uniq.contains(&d.key));
    }
    log::emph(&format!(
        "dotfiles details for profile \"{}\":\n\n",
        opts.profile
    ));
    for d in &dotfiles {
        detail(&opts.settings.dotpath, d);
    }
    log::log("");
}

////////////////////////////////////////////////////////////
// uninstall / remove
////////////////////////////////////////////////////////////

pub fn cmd_uninstall(opts: &Options, conf: &CfgAggregator) -> bool {
    let mut dotfiles = opts.dotfiles.clone();
    if !opts.uninstall_key.is_empty() {
        dotfiles = uniq_list(&opts.uninstall_key)
            .iter()
            .filter_map(|k| conf.get_dotfile(k, None))
            .collect();
    }
    if dotfiles.is_empty() {
        log::warn(format!(
            "no dotfile to uninstall for this profile (\"{}\")",
            opts.profile
        ));
        return false;
    }
    dbg_log!(
        "dotfiles registered for uninstall: {:?}",
        dotfiles.iter().map(|d| &d.key).collect::<Vec<_>>()
    );
    let uninst = Uninstaller::new(opts.dry, opts.safe, BACKUP_SUFFIX);
    let mut uninstalled = 0;
    for d in &dotfiles {
        let (res, msg) = uninst.uninstall(&d.src, &d.dst, d.link);
        if !res {
            log::err(msg.unwrap_or_default());
            continue;
        }
        uninstalled += 1;
    }
    log::log(&format!("\n{uninstalled} dotfile(s) uninstalled."));
    true
}

pub fn cmd_remove(opts: &Options, conf: &mut CfgAggregator) -> Result<bool> {
    if opts.remove_path.is_empty() {
        log::log("no dotfile to remove");
        return Ok(false);
    }
    dbg_log!("dotfile(s) to remove: {}", opts.remove_path.join(","));
    let mut removed: Vec<std::sync::Arc<Dotfile>> = Vec::new();
    for key in &opts.remove_path {
        let dotfiles = if !opts.remove_iskey {
            let d = conf.get_dotfile_by_dst(key, None);
            if d.is_empty() {
                log::warn(format!("{key} ignored, does not exist"));
                continue;
            }
            d
        } else {
            match conf.get_dotfile(key, None) {
                Some(d) => vec![d],
                None => {
                    log::warn(format!("{key} ignored, does not exist"));
                    continue;
                }
            }
        };
        for dotfile in dotfiles {
            let k = &dotfile.key;
            if dotfile.link != LinkType::NoLink {
                log::warn(format!("{k} uses symlink, remove manually"));
                continue;
            }
            dbg_log!("removing {key}");
            if !opts.dotfiles.iter().any(|d| d.key == *k) {
                log::warn(format!("{key} ignored, not associated to this profile"));
                continue;
            }
            let profiles: Vec<_> = conf
                .get_profiles_by_dotfile_key(k)
                .iter()
                .map(|p| (*p).clone())
                .collect();
            let pkeys = profiles
                .iter()
                .map(|p| p.key.clone())
                .collect::<Vec<_>>()
                .join(",");
            if opts.dry {
                log::dry(&format!("would remove {dotfile} from {pkeys}"));
                continue;
            }
            if opts.safe && !log::ask(&format!("Remove \"{k}\" from all these profiles: {pkeys}")) {
                return Ok(false);
            }
            dbg_log!("remove dotfile: {dotfile}");
            for p in &profiles {
                if !conf.del_dotfile_from_profile(&dotfile, p) {
                    return Ok(false);
                }
            }
            if !conf.del_dotfile(&dotfile) {
                return Ok(false);
            }

            // remove dotfile from dotpath
            let dtpath = join(&opts.settings.dotpath, &dotfile.src);
            removepath(&dtpath);
            // remove any empty parent up to dotpath
            let mut parent = utils::dirname(&dtpath);
            while parent != opts.settings.dotpath && parent.len() > 1 {
                if utils::is_dir(&parent) && dir_empty(&parent) {
                    if opts.safe && !log::ask(&format!("Remove empty dir \"{parent}\"")) {
                        break;
                    }
                    if !removepath(&parent) {
                        log::warn(format!("unable to remove {parent}"));
                    }
                }
                parent = utils::dirname(&parent);
            }
            removed.push(dotfile.clone());
        }
    }
    if opts.dry {
        log::dry("new config file would be:");
        log::raw(&conf.dump()?);
    } else {
        conf.save()?;
    }
    if removed.is_empty() {
        log::log("\nno dotfile removed");
    } else {
        log::log("\nFollowing dotfile(s) are not tracked anymore:");
        let entries: Vec<String> = removed
            .iter()
            .map(|r| format!("- \"{}\" (was tracked as \"{}\")", r.dst, r.key))
            .collect();
        log::log(&entries.join("\n"));
    }
    Ok(true)
}
