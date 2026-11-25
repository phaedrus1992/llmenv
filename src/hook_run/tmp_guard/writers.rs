//! The commands that write a file or a folder through an option, for the `/tmp` guard (#4053):
//! `git clone`, `curl`, `wget`, `dd`, and `tar`.
//!
//! Each function returns the paths that the command writes. A path that the command only reads
//! is not returned.

/// The paths that `name` writes through its options, or an empty list for a command that has no
/// such rule here. `args` are the words after the program name.
pub(super) fn written_by_option<'a>(name: &str, args: &[&'a str]) -> Vec<&'a str> {
    match name {
        "git" => git_clone(args),
        "curl" => option_values(args, &['o'], &["--output", "--output-dir"]),
        "wget" => option_values(
            args,
            &['O', 'P'],
            &["--output-document", "--directory-prefix"],
        ),
        "dd" => args.iter().filter_map(|a| a.strip_prefix("of=")).collect(),
        "tar" => tar_extract_dir(args),
        _ => Vec::new(),
    }
}

/// The values of the options in `short` (one letter each) and `long` in `args`. A value can stand
/// in the next word, as in `-o FILE` and `--output FILE`, or be attached, as in `-oFILE`,
/// `-sSoFILE`, and `--output=FILE`. A letter inside a cluster of short options takes the rest of
/// the cluster, or the next word when the cluster ends there.
pub(super) fn option_values<'a>(args: &[&'a str], short: &[char], long: &[&str]) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, attached) = rest
                .split_once('=')
                .map_or((rest, None), |(n, v)| (n, Some(v)));
            if long.iter().any(|l| l.strip_prefix("--") == Some(name)) {
                values.extend(attached.or_else(|| iter.next()));
            }
        } else if let Some(cluster) = arg.strip_prefix('-') {
            let at = cluster.char_indices().find(|(_, c)| short.contains(c));
            if let Some((i, c)) = at {
                let rest = &cluster[i + c.len_utf8()..];
                values.extend(if rest.is_empty() {
                    iter.next()
                } else {
                    Some(rest)
                });
            }
        }
    }
    values
}

/// The options of `git clone` that take a value in the next word, so the value is not read as an
/// operand.
const CLONE_VALUE_OPTIONS: [&str; 17] = [
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
    "--separate-git-dir",
    "--filter",
    "--shallow-since",
    "--shallow-exclude",
    "-u",
];

/// The folders that `git clone` writes: the destination operand, the `-C` folder that the clone
/// runs in, and the `--separate-git-dir` folder.
fn git_clone<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut written = Vec::new();
    let mut iter = args.iter().copied();
    let mut run_in = None;
    // The words before the subcommand: `-C DIR` and `-c KEY=VALUE` take the next word.
    let mut cloning = false;
    while let Some(arg) = iter.next() {
        match arg {
            "-C" => run_in = iter.next(),
            "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => {
                iter.next();
            }
            "clone" => {
                cloning = true;
                break;
            }
            a if a.starts_with('-') => {}
            _ => return written,
        }
    }
    if !cloning {
        return written;
    }
    written.extend(run_in);
    let mut operands = Vec::new();
    while let Some(arg) = iter.next() {
        if arg == "--separate-git-dir" {
            written.extend(iter.next());
        } else if let Some(dir) = arg.strip_prefix("--separate-git-dir=") {
            written.push(dir);
        } else if CLONE_VALUE_OPTIONS.contains(&arg) {
            iter.next();
        } else if !arg.starts_with('-') {
            operands.push(arg);
        }
    }
    // With one operand, the clone goes to a new folder in the working folder.
    written.extend(operands.get(1));
    written
}

/// The `-C` folder of a `tar` that extracts. A `tar` that creates an archive reads from that
/// folder, so it writes nothing there.
fn tar_extract_dir<'a>(args: &[&'a str]) -> Vec<&'a str> {
    if !tar_extracts(args) {
        return Vec::new();
    }
    option_values(args, &['C'], &["--directory"])
}

/// Whether the `tar` command line asks to extract: `-x`, `--extract`, `--get`, or a first word
/// such as `xzf` that has no dash.
fn tar_extracts(args: &[&str]) -> bool {
    args.iter()
        .enumerate()
        .any(|(i, arg)| match arg.strip_prefix("--") {
            Some(long) => matches!(long, "extract" | "get"),
            None => match arg.strip_prefix('-') {
                Some(cluster) => cluster.contains('x'),
                // A first word with no dash is the old style: `tar xzf file`.
                None => i == 0 && arg.contains('x'),
            },
        })
}
