// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.
// spell-checker:ignore (vars) RFILE
#![cfg(target_os = "linux")]
#![allow(clippy::upper_case_acronyms)]

use clap::builder::ValueParser;
use uucore::error::{UResult, USimpleError, UUsageError};
use uucore::{display::Quotable, format_usage, show_error, show_warning};

use clap::{Arg, ArgAction, Command};
use selinux::{OpaqueSecurityContext, SecurityContext};

use std::borrow::Cow;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::{fs, io};

mod errors;
mod fts;

use errors::*;

use uucore::locale::get_message;

pub mod options {
    pub static HELP: &str = "help";
    pub static VERBOSE: &str = "verbose";

    pub static REFERENCE: &str = "reference";

    pub static USER: &str = "user";
    pub static ROLE: &str = "role";
    pub static TYPE: &str = "type";
    pub static RANGE: &str = "range";

    pub static RECURSIVE: &str = "recursive";

    pub mod sym_links {
        pub static FOLLOW_ARG_DIR_SYM_LINK: &str = "follow-arg-dir-sym-link";
        pub static FOLLOW_DIR_SYM_LINKS: &str = "follow-dir-sym-links";
        pub static NO_FOLLOW_SYM_LINKS: &str = "no-follow-sym-links";
    }

    pub mod dereference {
        pub static DEREFERENCE: &str = "dereference";
        pub static NO_DEREFERENCE: &str = "no-dereference";
    }

    pub mod preserve_root {
        pub static PRESERVE_ROOT: &str = "preserve-root";
        pub static NO_PRESERVE_ROOT: &str = "no-preserve-root";
    }
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let config = uu_app();

    let options = match parse_command_line(config, args) {
        Ok(r) => r,
        Err(r) => {
            if let Error::CommandLine(r) = r {
                return Err(r.into());
            }

            return Err(UUsageError::new(libc::EXIT_FAILURE, format!("{r}.\n")));
        }
    };

    let context = match &options.mode {
        CommandLineMode::ReferenceBased { reference } => {
            let result = match SecurityContext::of_path(reference, true, false) {
                Ok(Some(context)) => Ok(context),

                Ok(None) => {
                    let err = io::Error::from_raw_os_error(libc::ENODATA);
                    Err(Error::from_io1("Getting security context", reference, err))
                }

                Err(r) => Err(Error::from_selinux("Getting security context", r)),
            };

            match result {
                Err(r) => {
                    return Err(USimpleError::new(
                        libc::EXIT_FAILURE,
                        format!("{}.", report_full_error(&r)),
                    ));
                }

                Ok(file_context) => SELinuxSecurityContext::File(file_context),
            }
        }

        CommandLineMode::ContextBased { context } => {
            let c_context = match os_str_to_c_string(context) {
                Ok(context) => context,

                Err(_r) => {
                    return Err(USimpleError::new(
                        libc::EXIT_FAILURE,
                        format!("Invalid security context {}.", context.quote()),
                    ));
                }
            };

            if SecurityContext::from_c_str(&c_context, false).check() == Some(false) {
                return Err(USimpleError::new(
                    libc::EXIT_FAILURE,
                    format!("Invalid security context {}.", context.quote()),
                ));
            }

            SELinuxSecurityContext::String(Some(c_context))
        }

        CommandLineMode::Custom { .. } => SELinuxSecurityContext::String(None),
    };

    let root_dev_ino = if options.preserve_root && options.recursive_mode.is_recursive() {
        match get_root_dev_ino() {
            Ok(r) => Some(r),

            Err(r) => {
                return Err(USimpleError::new(
                    libc::EXIT_FAILURE,
                    format!("{}.", report_full_error(&r)),
                ));
            }
        }
    } else {
        None
    };

    let results = process_files(&options, &context, root_dev_ino);
    if results.is_empty() {
        return Ok(());
    }

    for result in &results {
        show_error!("{}.", report_full_error(result));
    }
    Err(libc::EXIT_FAILURE.into())
}

pub fn uu_app() -> Command {
    Command::new(uucore::util_name())
        .version(uucore::crate_version!())
        .about(get_message("chcon-about"))
        .override_usage(format_usage(&get_message("chcon-usage")))
        .infer_long_args(true)
        .disable_help_flag(true)
        .args_override_self(true)
        .arg(
            Arg::new(options::HELP)
                .long(options::HELP)
                .help("Print help information.")
                .action(ArgAction::Help),
        )
        .arg(
            Arg::new(options::dereference::DEREFERENCE)
                .long(options::dereference::DEREFERENCE)
                .overrides_with(options::dereference::NO_DEREFERENCE)
                .help(
                    "Affect the referent of each symbolic link (this is the default), \
                     rather than the symbolic link itself.",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::dereference::NO_DEREFERENCE)
                .short('h')
                .long(options::dereference::NO_DEREFERENCE)
                .help("Affect symbolic links instead of any referenced file.")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::preserve_root::PRESERVE_ROOT)
                .long(options::preserve_root::PRESERVE_ROOT)
                .overrides_with(options::preserve_root::NO_PRESERVE_ROOT)
                .help("Fail to operate recursively on '/'.")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::preserve_root::NO_PRESERVE_ROOT)
                .long(options::preserve_root::NO_PRESERVE_ROOT)
                .help("Do not treat '/' specially (the default).")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::REFERENCE)
                .long(options::REFERENCE)
                .value_name("RFILE")
                .value_hint(clap::ValueHint::FilePath)
                .conflicts_with_all([options::USER, options::ROLE, options::TYPE, options::RANGE])
                .help(
                    "Use security context of RFILE, rather than specifying \
                     a CONTEXT value.",
                )
                .value_parser(ValueParser::os_string()),
        )
        .arg(
            Arg::new(options::USER)
                .short('u')
                .long(options::USER)
                .value_name("USER")
                .value_hint(clap::ValueHint::Username)
                .help("Set user USER in the target security context.")
                .value_parser(ValueParser::os_string()),
        )
        .arg(
            Arg::new(options::ROLE)
                .short('r')
                .long(options::ROLE)
                .value_name("ROLE")
                .help("Set role ROLE in the target security context.")
                .value_parser(ValueParser::os_string()),
        )
        .arg(
            Arg::new(options::TYPE)
                .short('t')
                .long(options::TYPE)
                .value_name("TYPE")
                .help("Set type TYPE in the target security context.")
                .value_parser(ValueParser::os_string()),
        )
        .arg(
            Arg::new(options::RANGE)
                .short('l')
                .long(options::RANGE)
                .value_name("RANGE")
                .help("Set range RANGE in the target security context.")
                .value_parser(ValueParser::os_string()),
        )
        .arg(
            Arg::new(options::RECURSIVE)
                .short('R')
                .long(options::RECURSIVE)
                .help("Operate on files and directories recursively.")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::sym_links::FOLLOW_ARG_DIR_SYM_LINK)
                .short('H')
                .requires(options::RECURSIVE)
                .overrides_with_all([
                    options::sym_links::FOLLOW_DIR_SYM_LINKS,
                    options::sym_links::NO_FOLLOW_SYM_LINKS,
                ])
                .help(
                    "If a command line argument is a symbolic link to a directory, \
                     traverse it. Only valid when -R is specified.",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::sym_links::FOLLOW_DIR_SYM_LINKS)
                .short('L')
                .requires(options::RECURSIVE)
                .overrides_with_all([
                    options::sym_links::FOLLOW_ARG_DIR_SYM_LINK,
                    options::sym_links::NO_FOLLOW_SYM_LINKS,
                ])
                .help(
                    "Traverse every symbolic link to a directory encountered. \
                     Only valid when -R is specified.",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::sym_links::NO_FOLLOW_SYM_LINKS)
                .short('P')
                .requires(options::RECURSIVE)
                .overrides_with_all([
                    options::sym_links::FOLLOW_ARG_DIR_SYM_LINK,
                    options::sym_links::FOLLOW_DIR_SYM_LINKS,
                ])
                .help(
                    "Do not traverse any symbolic links (default). \
                     Only valid when -R is specified.",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::VERBOSE)
                .short('v')
                .long(options::VERBOSE)
                .help("Output a diagnostic for every file processed.")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("FILE")
                .action(ArgAction::Append)
                .value_hint(clap::ValueHint::FilePath)
                .num_args(1..)
                .value_parser(ValueParser::os_string()),
        )
}

#[derive(Debug)]
struct Options {
    verbose: bool,
    preserve_root: bool,
    recursive_mode: RecursiveMode,
    affect_symlink_referent: bool,
    mode: CommandLineMode,
    files: Vec<PathBuf>,
}

fn parse_command_line(config: Command, args: impl uucore::Args) -> Result<Options> {
    let matches = config.try_get_matches_from(args)?;

    let verbose = matches.get_flag(options::VERBOSE);

    let (recursive_mode, affect_symlink_referent) = if matches.get_flag(options::RECURSIVE) {
        if matches.get_flag(options::sym_links::FOLLOW_DIR_SYM_LINKS) {
            if matches.get_flag(options::dereference::NO_DEREFERENCE) {
                return Err(Error::ArgumentsMismatch(format!(
                    "'--{}' with '--{}' require '-P'",
                    options::RECURSIVE,
                    options::dereference::NO_DEREFERENCE
                )));
            }

            (RecursiveMode::RecursiveAndFollowAllDirSymLinks, true)
        } else if matches.get_flag(options::sym_links::FOLLOW_ARG_DIR_SYM_LINK) {
            if matches.get_flag(options::dereference::NO_DEREFERENCE) {
                return Err(Error::ArgumentsMismatch(format!(
                    "'--{}' with '--{}' require '-P'",
                    options::RECURSIVE,
                    options::dereference::NO_DEREFERENCE
                )));
            }

            (RecursiveMode::RecursiveAndFollowArgDirSymLinks, true)
        } else {
            if matches.get_flag(options::dereference::DEREFERENCE) {
                return Err(Error::ArgumentsMismatch(format!(
                    "'--{}' with '--{}' require either '-H' or '-L'",
                    options::RECURSIVE,
                    options::dereference::DEREFERENCE
                )));
            }

            (RecursiveMode::RecursiveButDoNotFollowSymLinks, false)
        }
    } else {
        let no_dereference = matches.get_flag(options::dereference::NO_DEREFERENCE);
        (RecursiveMode::NotRecursive, !no_dereference)
    };

    // By default, do not preserve root.
    let preserve_root = matches.get_flag(options::preserve_root::PRESERVE_ROOT);

    let mut files = matches.get_many::<OsString>("FILE").unwrap_or_default();

    let mode = if let Some(path) = matches.get_one::<OsString>(options::REFERENCE) {
        CommandLineMode::ReferenceBased {
            reference: PathBuf::from(path),
        }
    } else if matches.contains_id(options::USER)
        || matches.contains_id(options::ROLE)
        || matches.contains_id(options::TYPE)
        || matches.contains_id(options::RANGE)
    {
        CommandLineMode::Custom {
            user: matches.get_one::<OsString>(options::USER).map(Into::into),
            role: matches.get_one::<OsString>(options::ROLE).map(Into::into),
            the_type: matches.get_one::<OsString>(options::TYPE).map(Into::into),
            range: matches.get_one::<OsString>(options::RANGE).map(Into::into),
        }
    } else if let Some(context) = files.next() {
        CommandLineMode::ContextBased {
            context: context.into(),
        }
    } else {
        return Err(Error::MissingContext);
    };

    let files: Vec<_> = files.map(PathBuf::from).collect();
    if files.is_empty() {
        return Err(Error::MissingFiles);
    }

    Ok(Options {
        verbose,
        preserve_root,
        recursive_mode,
        affect_symlink_referent,
        mode,
        files,
    })
}

#[derive(Debug, Copy, Clone)]
enum RecursiveMode {
    NotRecursive,
    /// Do not traverse any symbolic links.
    RecursiveButDoNotFollowSymLinks,
    /// Traverse every symbolic link to a directory encountered.
    RecursiveAndFollowAllDirSymLinks,
    /// If a command line argument is a symbolic link to a directory, traverse it.
    RecursiveAndFollowArgDirSymLinks,
}

impl RecursiveMode {
    fn is_recursive(self) -> bool {
        match self {
            Self::NotRecursive => false,

            Self::RecursiveButDoNotFollowSymLinks
            | Self::RecursiveAndFollowAllDirSymLinks
            | Self::RecursiveAndFollowArgDirSymLinks => true,
        }
    }

    fn fts_open_options(self) -> c_int {
        match self {
            Self::NotRecursive | Self::RecursiveButDoNotFollowSymLinks => fts_sys::FTS_PHYSICAL,

            Self::RecursiveAndFollowAllDirSymLinks => fts_sys::FTS_LOGICAL,

            Self::RecursiveAndFollowArgDirSymLinks => {
                fts_sys::FTS_PHYSICAL | fts_sys::FTS_COMFOLLOW
            }
        }
    }
}

#[derive(Debug)]
enum CommandLineMode {
    ReferenceBased {
        reference: PathBuf,
    },
    ContextBased {
        context: OsString,
    },
    Custom {
        user: Option<OsString>,
        role: Option<OsString>,
        the_type: Option<OsString>,
        range: Option<OsString>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeviceAndINode {
    device_id: u64,
    inode: u64,
}

#[cfg(unix)]
impl From<fs::Metadata> for DeviceAndINode {
    fn from(md: fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device_id: md.dev(),
            inode: md.ino(),
        }
    }
}

impl TryFrom<&libc::stat> for DeviceAndINode {
    type Error = Error;

    #[allow(clippy::useless_conversion)]
    fn try_from(st: &libc::stat) -> Result<Self> {
        let device_id = u64::try_from(st.st_dev).map_err(|_r| Error::OutOfRange)?;
        let inode = u64::try_from(st.st_ino).map_err(|_r| Error::OutOfRange)?;
        Ok(Self { device_id, inode })
    }
}

fn process_files(
    options: &Options,
    context: &SELinuxSecurityContext,
    root_dev_ino: Option<DeviceAndINode>,
) -> Vec<Error> {
    let fts_options = options.recursive_mode.fts_open_options();
    let mut fts = match fts::FTS::new(options.files.iter(), fts_options) {
        Ok(fts) => fts,
        Err(err) => return vec![err],
    };

    let mut errors = Vec::default();
    loop {
        match fts.read_next_entry() {
            Ok(true) => {
                if let Err(err) = process_file(options, context, &mut fts, root_dev_ino) {
                    errors.push(err);
                }
            }

            Ok(false) => break,

            Err(err) => {
                errors.push(err);
                break;
            }
        }
    }
    errors
}

fn process_file(
    options: &Options,
    context: &SELinuxSecurityContext,
    fts: &mut fts::FTS,
    root_dev_ino: Option<DeviceAndINode>,
) -> Result<()> {
    let mut entry = fts.last_entry_ref().unwrap();

    let file_full_name = entry.path().map(PathBuf::from).ok_or_else(|| {
        Error::from_io("File name validation", io::ErrorKind::InvalidInput.into())
    })?;

    let fts_access_path = entry.access_path().ok_or_else(|| {
        let err = io::ErrorKind::InvalidInput.into();
        Error::from_io1("File name validation", &file_full_name, err)
    })?;

    let err = |s, k: io::ErrorKind| Error::from_io1(s, &file_full_name, k.into());

    let fts_err = |s| {
        let r = io::Error::from_raw_os_error(entry.errno());
        Err(Error::from_io1(s, &file_full_name, r))
    };

    // SAFETY: If `entry.fts_statp` is not null, then is is assumed to be valid.
    let file_dev_ino: DeviceAndINode = if let Some(st) = entry.stat() {
        st.try_into()?
    } else {
        return Err(err("Getting meta data", io::ErrorKind::InvalidInput));
    };

    let mut result = Ok(());

    match entry.flags() {
        fts_sys::FTS_D => {
            if options.recursive_mode.is_recursive() {
                if root_dev_ino_check(root_dev_ino, file_dev_ino) {
                    // This happens e.g., with "chcon -R --preserve-root ... /"
                    // and with "chcon -RH --preserve-root ... symlink-to-root".
                    root_dev_ino_warn(&file_full_name);

                    // Tell fts not to traverse into this hierarchy.
                    let _ignored = fts.set(fts_sys::FTS_SKIP);

                    // Ensure that we do not process "/" on the second visit.
                    let _ignored = fts.read_next_entry();

                    return Err(err("Modifying root path", io::ErrorKind::PermissionDenied));
                }

                return Ok(());
            }
        }

        fts_sys::FTS_DP => {
            if !options.recursive_mode.is_recursive() {
                return Ok(());
            }
        }

        fts_sys::FTS_NS => {
            // For a top-level file or directory, this FTS_NS (stat failed) indicator is determined
            // at the time of the initial fts_open call. With programs like chmod, chown, and chgrp,
            // that modify permissions, it is possible that the file in question is accessible when
            // control reaches this point. So, if this is the first time we've seen the FTS_NS for
            // this file, tell fts_read to stat it "again".
            if entry.level() == 0 && entry.number() == 0 {
                entry.set_number(1);
                let _ignored = fts.set(fts_sys::FTS_AGAIN);
                return Ok(());
            }

            result = fts_err("Accessing");
        }

        fts_sys::FTS_ERR => result = fts_err("Accessing"),

        fts_sys::FTS_DNR => result = fts_err("Reading directory"),

        fts_sys::FTS_DC => {
            if cycle_warning_required(options.recursive_mode.fts_open_options(), &entry) {
                emit_cycle_warning(&file_full_name);
                return Err(err("Reading cyclic directory", io::ErrorKind::InvalidData));
            }
        }

        _ => {}
    }

    if entry.flags() == fts_sys::FTS_DP
        && result.is_ok()
        && root_dev_ino_check(root_dev_ino, file_dev_ino)
    {
        root_dev_ino_warn(&file_full_name);
        result = Err(err("Modifying root path", io::ErrorKind::PermissionDenied));
    }

    if result.is_ok() {
        if options.verbose {
            println!(
                "{}: changing security context of {}",
                uucore::util_name(),
                file_full_name.quote()
            );
        }

        result = change_file_context(options, context, fts_access_path);
    }

    if !options.recursive_mode.is_recursive() {
        let _ignored = fts.set(fts_sys::FTS_SKIP);
    }
    result
}

fn change_file_context(
    options: &Options,
    context: &SELinuxSecurityContext,
    path: &Path,
) -> Result<()> {
    match &options.mode {
        CommandLineMode::Custom {
            user,
            role,
            the_type,
            range,
        } => {
            let err0 = || -> Result<()> {
                // If the file doesn't have a context, and we're not setting all of the context
                // components, there isn't really an obvious default. Thus, we just give up.
                let op = "Applying partial security context to unlabeled file";
                let err = io::ErrorKind::InvalidInput.into();
                Err(Error::from_io1(op, path, err))
            };

            let file_context =
                match SecurityContext::of_path(path, options.affect_symlink_referent, false) {
                    Ok(Some(context)) => context,

                    Ok(None) => return err0(),
                    Err(r) => return Err(Error::from_selinux("Getting security context", r)),
                };

            let c_file_context = match file_context.to_c_string() {
                Ok(Some(context)) => context,

                Ok(None) => return err0(),
                Err(r) => return Err(Error::from_selinux("Getting security context", r)),
            };

            let se_context =
                OpaqueSecurityContext::from_c_str(c_file_context.as_ref()).map_err(|_r| {
                    let err = io::ErrorKind::InvalidInput.into();
                    Error::from_io1("Creating security context", path, err)
                })?;

            type SetValueProc = fn(&OpaqueSecurityContext, &CStr) -> selinux::errors::Result<()>;

            let list: &[(&Option<OsString>, SetValueProc)] = &[
                (user, OpaqueSecurityContext::set_user),
                (role, OpaqueSecurityContext::set_role),
                (the_type, OpaqueSecurityContext::set_type),
                (range, OpaqueSecurityContext::set_range),
            ];

            for (new_value, set_value_proc) in list {
                if let Some(new_value) = new_value {
                    let c_new_value = os_str_to_c_string(new_value).map_err(|_r| {
                        let err = io::ErrorKind::InvalidInput.into();
                        Error::from_io1("Creating security context", path, err)
                    })?;

                    set_value_proc(&se_context, &c_new_value)
                        .map_err(|r| Error::from_selinux("Setting security context user", r))?;
                }
            }

            let context_string = se_context
                .to_c_string()
                .map_err(|r| Error::from_selinux("Getting security context", r))?;

            if c_file_context.as_ref().to_bytes() == context_string.as_ref().to_bytes() {
                Ok(()) // Nothing to change.
            } else {
                SecurityContext::from_c_str(&context_string, false)
                    .set_for_path(path, options.affect_symlink_referent, false)
                    .map_err(|r| Error::from_selinux("Setting security context", r))
            }
        }

        CommandLineMode::ReferenceBased { .. } | CommandLineMode::ContextBased { .. } => {
            if let Some(c_context) = context.to_c_string()? {
                SecurityContext::from_c_str(c_context.as_ref(), false)
                    .set_for_path(path, options.affect_symlink_referent, false)
                    .map_err(|r| Error::from_selinux("Setting security context", r))
            } else {
                let err = io::ErrorKind::InvalidInput.into();
                Err(Error::from_io1("Setting security context", path, err))
            }
        }
    }
}

#[cfg(unix)]
pub(crate) fn os_str_to_c_string(s: &OsStr) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;

    CString::new(s.as_bytes())
        .map_err(|_r| Error::from_io("CString::new()", io::ErrorKind::InvalidInput.into()))
}

/// Call `lstat()` to get the device and inode numbers for `/`.
#[cfg(unix)]
fn get_root_dev_ino() -> Result<DeviceAndINode> {
    fs::symlink_metadata("/")
        .map(DeviceAndINode::from)
        .map_err(|r| Error::from_io1("std::fs::symlink_metadata", "/", r))
}

fn root_dev_ino_check(root_dev_ino: Option<DeviceAndINode>, dir_dev_ino: DeviceAndINode) -> bool {
    root_dev_ino == Some(dir_dev_ino)
}

fn root_dev_ino_warn(dir_name: &Path) {
    if dir_name.as_os_str() == "/" {
        show_warning!(
            "It is dangerous to operate recursively on '/'. \
             Use --{} to override this failsafe.",
            options::preserve_root::NO_PRESERVE_ROOT,
        );
    } else {
        show_warning!(
            "It is dangerous to operate recursively on {} (same as '/'). \
             Use --{} to override this failsafe.",
            dir_name.quote(),
            options::preserve_root::NO_PRESERVE_ROOT,
        );
    }
}

// When fts_read returns FTS_DC to indicate a directory cycle, it may or may not indicate
// a real problem.
// When a program like chgrp performs a recursive traversal that requires traversing symbolic links,
// it is *not* a problem.
// However, when invoked with "-P -R", it deserves a warning.
// The fts_options parameter records the options that control this aspect of fts behavior,
// so test that.
fn cycle_warning_required(fts_options: c_int, entry: &fts::EntryRef) -> bool {
    // When dereferencing no symlinks, or when dereferencing only those listed on the command line
    // and we're not processing a command-line argument, then a cycle is a serious problem.
    ((fts_options & fts_sys::FTS_PHYSICAL) != 0)
        && (((fts_options & fts_sys::FTS_COMFOLLOW) == 0) || entry.level() != 0)
}

fn emit_cycle_warning(file_name: &Path) {
    show_warning!(
        "Circular directory structure.\n\
This almost certainly means that you have a corrupted file system.\n\
NOTIFY YOUR SYSTEM MANAGER.\n\
The following directory is part of the cycle {}.",
        file_name.quote()
    );
}

#[derive(Debug)]
enum SELinuxSecurityContext<'t> {
    File(SecurityContext<'t>),
    String(Option<CString>),
}

impl SELinuxSecurityContext<'_> {
    fn to_c_string(&self) -> Result<Option<Cow<CStr>>> {
        match self {
            Self::File(context) => context
                .to_c_string()
                .map_err(|r| Error::from_selinux("SELinuxSecurityContext::to_c_string()", r)),

            Self::String(context) => Ok(context.as_deref().map(Cow::Borrowed)),
        }
    }
}
