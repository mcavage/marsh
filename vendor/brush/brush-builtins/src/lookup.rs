//! Shared command-name resolution, used by the `type` and `command` builtins.

use std::io::Write;
use std::path::{Path, PathBuf};

use brush_core::{
    Shell, ShellExtensions,
    parser::ast,
    pathsearch,
    sys::{self, fs::PathExt},
};

/// A way in which a name resolved, in the shell's lookup order.
pub(crate) enum Resolved<'a> {
    /// An alias, with its target.
    Alias(String),
    /// A shell keyword.
    Keyword,
    /// A shell function, with its definition.
    Function(&'a ast::FunctionDefinition),
    /// A built-in command.
    Builtin,
    /// An executable file; `hashed` indicates it came from the program location cache.
    File { path: PathBuf, hashed: bool },
}

/// Options for [`resolve`]; the defaults match a plain `type NAME` lookup.
#[derive(Default)]
pub(crate) struct Options {
    /// Only search the filesystem, even if the name is an alias, keyword, function, or builtin.
    pub force_path_search: bool,
    /// Don't consider functions when resolving the name.
    pub suppress_func_lookup: bool,
    /// Report every location the name resolves to, not just the first.
    pub all_locations: bool,
    /// Directories to search for executables, in lieu of the shell's `PATH`. The shell's
    /// hash-based path cache is still consulted first.
    pub path_dirs: Option<Vec<PathBuf>>,
}

/// Resolves the given name, returning the ways it resolved (in lookup order). Unless
/// `all_locations` was requested, at most one way is returned.
pub(crate) fn resolve<'a, SE: ShellExtensions>(
    shell: &'a Shell<SE>,
    name: &str,
    options: &Options,
) -> Vec<Resolved<'a>> {
    let mut resolved = vec![];

    // These are all hash lookups, so there's nothing to save by stopping at the first hit;
    // any extras are trimmed off at the end.
    if !options.force_path_search {
        // Check for aliases. They're only reported when alias expansion is enabled, since
        // that's the only case in which the name would actually resolve to the alias.
        if shell.options().expand_aliases
            && let Some(target) = shell.aliases().get(name)
        {
            resolved.push(Resolved::Alias(target.clone()));
        }

        // Check for keywords.
        if shell.is_keyword(name) {
            resolved.push(Resolved::Keyword);
        }

        // Check for functions.
        if !options.suppress_func_lookup
            && let Some(registration) = shell.funcs().get(name)
        {
            resolved.push(Resolved::Function(registration.definition()));
        }

        // Check for builtins. A process-backed registration (an marsh
        // registered command) runs as a separate process, so it is described
        // as the file on PATH that reaches it, when there is one.
        if let Some(builtin) = shell.builtins().get(name).filter(|b| !b.disabled) {
            match builtin
                .process_shim
                .as_ref()
                .and_then(|shim| shim_path(shell, name, &shim.executable))
            {
                Some(path) => resolved.push(Resolved::File {
                    path,
                    hashed: false,
                }),
                None => resolved.push(Resolved::Builtin),
            }
        }
    }

    // Searching the filesystem *does* cost something, so only do it if the results so far
    // don't already answer the question.
    if options.all_locations || resolved.is_empty() {
        let found = resolved.len();
        resolve_in_filesystem(shell, name, options, &mut resolved);
        // A registered command's PATH link was already reported above.
        let earlier: Vec<PathBuf> = resolved[..found]
            .iter()
            .filter_map(|resolved| match resolved {
                Resolved::File { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        let mut index = 0;
        resolved.retain(|resolved| {
            index += 1;
            index <= found
                || !matches!(resolved, Resolved::File { path, .. } if earlier.contains(path))
        });
    }

    if !options.all_locations {
        resolved.truncate(1);
    }

    resolved
}

/// Returns the first executable named `name` on `PATH` that is a link to a
/// process-backed registration's `executable`, as the marsh session command
/// directory provides for every registered command.
fn shim_path<SE: ShellExtensions>(
    shell: &Shell<SE>,
    name: &str,
    executable: &Path,
) -> Option<PathBuf> {
    let target = executable.canonicalize().ok()?;
    shell.find_executables_in_path(name).find(|candidate| {
        shell
            .absolute_path(candidate)
            .canonicalize()
            .is_ok_and(|resolved| resolved == target)
    })
}

/// Appends the files the given name resolves to.
fn resolve_in_filesystem<SE: ShellExtensions>(
    shell: &Shell<SE>,
    name: &str,
    options: &Options,
    resolved: &mut Vec<Resolved<'_>>,
) {
    let to_file = |path| Resolved::File {
        path,
        hashed: false,
    };

    // A name with a separator in it is used as-is; it's never searched for.
    if sys::fs::contains_path_separator(name) {
        // A directory is never a command, even though it carries the execute bit.
        let candidate = shell.absolute_path(Path::new(name));
        if !candidate.is_dir() && candidate.executable() {
            resolved.push(to_file(PathBuf::from(name)));
        }
        return;
    }

    // Reporting every location is a strict search for executables; reporting just the one the
    // name resolves to matches what the shell would actually try to run, which can be a
    // non-executable file if the search turns up nothing better.
    if let Some(path) = shell.program_location_cache().get(name) {
        resolved.push(Resolved::File { path, hashed: true });
        if !options.all_locations {
            return;
        }
    }

    match (&options.path_dirs, options.all_locations) {
        (Some(dirs), true) => {
            resolved.extend(pathsearch::search_for_executable(dirs.iter(), name).map(to_file));
        }
        (Some(dirs), false) => {
            resolved.extend(pathsearch::resolve_command(dirs.iter(), name).map(to_file));
        }
        (None, true) => resolved.extend(shell.find_executables_in_path(name).map(to_file)),
        (None, false) => resolved.extend(shell.resolve_command_in_path(name).map(to_file)),
    }
}

/// Writes the description shown by `type NAME` and `command -V NAME`, newline included.
pub(crate) fn describe(
    mut writer: impl Write,
    name: &str,
    resolved: &Resolved<'_>,
) -> std::io::Result<()> {
    match resolved {
        Resolved::Alias(target) => writeln!(writer, "{name} is aliased to `{target}'"),
        Resolved::Keyword => writeln!(writer, "{name} is a shell keyword"),
        Resolved::Function(def) => writeln!(writer, "{name} is a function\n{def}"),
        Resolved::Builtin => writeln!(writer, "{name} is a shell builtin"),
        Resolved::File { path, hashed } => {
            let path = path.to_string_lossy();
            if *hashed {
                writeln!(writer, "{name} is hashed ({path})")
            } else {
                writeln!(writer, "{name} is {path}")
            }
        }
    }
}
