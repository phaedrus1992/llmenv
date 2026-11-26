//! The commands that write a file or a folder through an option, for the `/tmp` guard (#4053):
//! `git clone`, `curl`, `wget`, `dd`, `tar`, and `unzip`.
//!
//! Each function returns the paths that the command writes. A path that the command only reads
//! is not returned.

/// What the option scan needs to know about one command.
struct Options {
    /// The short options whose value is a path that the command writes.
    write: &'static [char],
    /// The other short options that take a value. A value can look like an option letter, such as
    /// the `o` in `-uroot`, so the scan must skip it.
    other: &'static [char],
    /// The long options whose value is a path that the command writes.
    long: &'static [&'static str],
}

const CURL: Options = Options {
    write: &['o', 'D', 'c'],
    other: &[
        'A', 'b', 'C', 'd', 'e', 'E', 'F', 'H', 'K', 'm', 'P', 'Q', 'r', 'T', 'u', 'U', 'w', 'x',
        'X', 'y', 'Y', 'z',
    ],
    long: &["output", "output-dir", "dump-header", "cookie-jar"],
};

const WGET: Options = Options {
    write: &['O', 'P', 'o', 'a'],
    other: &[
        'A', 'B', 'D', 'e', 'i', 'I', 'l', 'Q', 't', 'T', 'U', 'w', 'X', 'Y',
    ],
    long: &[
        "output-document",
        "directory-prefix",
        "output-file",
        "append-output",
    ],
};

/// `unzip -d DIR` extracts into `DIR`. Its `-P` takes a password.
const UNZIP: Options = Options {
    write: &['d'],
    other: &['P'],
    long: &[],
};

/// The paths that `name` writes through its options, or an empty list for a command that has no
/// such rule here. `args` are the words after the program name.
pub(super) fn written_by_option(name: &str, args: &[&str]) -> Vec<String> {
    let owned = |values: Vec<&str>| values.into_iter().map(str::to_owned).collect();
    match name {
        "git" => git_clone(args),
        "curl" => owned(option_values(args, CURL.write, CURL.other, CURL.long)),
        "wget" => owned(option_values(args, WGET.write, WGET.other, WGET.long)),
        "dd" => args
            .iter()
            .filter_map(|a| a.strip_prefix("of="))
            .map(str::to_owned)
            .collect(),
        "tar" => tar_paths(args),
        "unzip" => owned(option_values(args, UNZIP.write, UNZIP.other, UNZIP.long)),
        _ => Vec::new(),
    }
}

/// Whether `typed` names the long option `full`. GNU `getopt_long` and git accept any unique
/// prefix of a long option, so a prefix counts. A prefix that is also the start of another option
/// counts too, because the guard cannot tell which one the command means.
fn names_long_option(typed: &str, full: &str) -> bool {
    typed.len() >= 2 && full.starts_with(typed)
}

/// The values of the options in `write` (one letter each) and `long` in `args`. A value can stand
/// in the next word, as in `-o FILE` and `--output FILE`, or be attached, as in `-oFILE`,
/// `-sSoFILE`, and `--output=FILE`. A letter in a cluster of short options takes the rest of the
/// cluster, or the next word when the cluster ends there. A letter in `other` takes the rest of
/// its cluster as its own value, so the scan stops there.
pub(super) fn option_values<'a>(
    args: &[&'a str],
    write: &[char],
    other: &[char],
    long: &[&str],
) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, attached) = rest
                .split_once('=')
                .map_or((rest, None), |(n, v)| (n, Some(v)));
            if long.iter().any(|full| names_long_option(name, full)) {
                values.extend(attached.or_else(|| iter.next()));
            }
        } else if let Some(cluster) = arg.strip_prefix('-') {
            for (i, c) in cluster.char_indices() {
                let rest = &cluster[i + c.len_utf8()..];
                if write.contains(&c) {
                    values.extend(if rest.is_empty() {
                        iter.next()
                    } else {
                        Some(rest)
                    });
                    break;
                }
                if other.contains(&c) {
                    // The rest of the cluster is the value, or the next word is, so it is not
                    // read as an option.
                    if rest.is_empty() {
                        iter.next();
                    }
                    break;
                }
            }
        }
    }
    values
}

/// The options of `git clone` that take a value in the next word, so the value is not read as an
/// operand.
const CLONE_VALUE_OPTIONS: [&str; 22] = [
    "-b",
    "--branch",
    "--depth",
    "-o",
    "--origin",
    "-c",
    "--config",
    "--reference",
    "--reference-if-able",
    "-j",
    "--jobs",
    "--template",
    "--filter",
    "--shallow-since",
    "--shallow-exclude",
    "-u",
    "--upload-pack",
    "--server-option",
    "--bundle-uri",
    "--ref-format",
    "--revision",
    "--separate-git-dir",
];

/// The global options of `git` that take a value in the next word, before the subcommand.
const GIT_GLOBAL_VALUE_OPTIONS: [&str; 7] = [
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
    "--attr-source",
];

/// `next` in the folder `base`: an absolute `next` stands alone, and a relative one is joined.
fn join_dir(base: Option<&str>, next: &str) -> String {
    match base {
        Some(base) if !next.starts_with('/') => format!("{base}/{next}"),
        _ => next.to_string(),
    }
}

/// The folders that `git clone` writes: the destination, and a `--separate-git-dir` folder. Each
/// `-C` folder moves the working folder, as git reads it: an absolute one replaces it, and a
/// relative one joins onto it.
fn git_clone(args: &[&str]) -> Vec<String> {
    let mut iter = args.iter().copied();
    let mut dir: Option<String> = None;
    loop {
        match iter.next() {
            Some("-C") => {
                let Some(next) = iter.next() else {
                    return Vec::new();
                };
                dir = Some(join_dir(dir.as_deref(), next));
            }
            Some(arg) if GIT_GLOBAL_VALUE_OPTIONS.contains(&arg) => {
                iter.next();
            }
            Some("clone") => break,
            Some(arg) if arg.starts_with('-') => {}
            _ => return Vec::new(),
        }
    }
    let mut written = Vec::new();
    let mut operands = Vec::new();
    while let Some(arg) = iter.next() {
        let long = arg.strip_prefix("--");
        let (name, attached) = long
            .and_then(|l| l.split_once('='))
            .map_or((long.unwrap_or(""), None), |(n, v)| (n, Some(v)));
        if long.is_some() && name.len() >= 3 && names_long_option(name, "separate-git-dir") {
            let value = attached.or_else(|| iter.next());
            written.extend(value.map(|v| join_dir(dir.as_deref(), v)));
        } else if CLONE_VALUE_OPTIONS.contains(&arg) {
            iter.next();
        } else if !arg.starts_with('-') {
            operands.push(arg);
        }
    }
    // The last operand is the destination when there are two. With one operand, the clone goes to
    // a new folder in the working folder. Taking the last one means that an option that is not in
    // the list cannot shift the destination.
    match (operands.len(), operands.last()) {
        (2.., Some(dest)) => written.push(join_dir(dir.as_deref(), dest)),
        _ => written.extend(dir),
    }
    written
}

/// The paths that `tar` writes: the archive of a command that creates, appends, or updates, and
/// the `-C` folder of a command that extracts. A `tar` that creates reads from its `-C` folder.
///
/// The first word can be the old style with no dash, as in `tar xzf a.tgz`. Its value letters take
/// the following words in the order of the letters.
fn tar_paths(args: &[&str]) -> Vec<String> {
    let mut found = TarFlags::default();
    let mut words = args.iter().copied().enumerate();
    while let Some((index, arg)) = words.next() {
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            if names_long_option(name, "extract") || name == "get" {
                found.extract = true;
            } else if ["create", "append", "update"]
                .iter()
                .any(|full| names_long_option(name, full))
            {
                found.archive = true;
            } else if names_long_option(name, "file") {
                found
                    .files
                    .extend(attached.or_else(|| words.next().map(|(_, w)| w)));
            } else if names_long_option(name, "directory") {
                found
                    .dirs
                    .extend(attached.or_else(|| words.next().map(|(_, w)| w)));
            }
        } else if let Some(cluster) = arg.strip_prefix('-') {
            found.cluster(cluster, &mut words);
        } else if index == 0 {
            found.old_style(arg, &mut words);
        }
    }
    let mut written: Vec<String> = Vec::new();
    if found.extract {
        written.extend(found.dirs.into_iter().map(str::to_owned));
    }
    if found.archive {
        written.extend(found.files.into_iter().map(str::to_owned));
    }
    written
}

/// What a scan of a `tar` command line found.
#[derive(Default)]
struct TarFlags<'a> {
    extract: bool,
    /// Creates, appends to, or updates an archive.
    archive: bool,
    files: Vec<&'a str>,
    dirs: Vec<&'a str>,
}

impl<'a> TarFlags<'a> {
    /// One cluster of short options, such as `xzf` or `Cdir`. A mode letter sets a flag, `f` and
    /// `C` take a value, and the other value letters skip theirs.
    fn cluster(&mut self, cluster: &'a str, words: &mut impl Iterator<Item = (usize, &'a str)>) {
        for (i, c) in cluster.char_indices() {
            let rest = &cluster[i + c.len_utf8()..];
            match c {
                'x' => self.extract = true,
                'c' | 'r' | 'u' => self.archive = true,
                'f' | 'C' => {
                    let found = if rest.is_empty() {
                        words.next().map(|(_, w)| w)
                    } else {
                        Some(rest)
                    };
                    let target = if c == 'f' {
                        &mut self.files
                    } else {
                        &mut self.dirs
                    };
                    target.extend(found);
                    return;
                }
                'T' | 'X' | 'I' | 'b' | 'L' | 'H' => {
                    if rest.is_empty() {
                        words.next();
                    }
                    return;
                }
                _ => {}
            }
        }
    }

    /// The first word of the old style, with no dash. Each value letter takes the next word.
    fn old_style(&mut self, letters: &'a str, words: &mut impl Iterator<Item = (usize, &'a str)>) {
        for c in letters.chars() {
            match c {
                'x' => self.extract = true,
                'c' | 'r' | 'u' => self.archive = true,
                'f' => self.files.extend(words.next().map(|(_, w)| w)),
                'C' => self.dirs.extend(words.next().map(|(_, w)| w)),
                'T' | 'X' | 'I' | 'b' | 'L' | 'H' => {
                    words.next();
                }
                _ => {}
            }
        }
    }
}
