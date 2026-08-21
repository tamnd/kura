//! Command line access to a kura index.
//!
//! Three commands, and only one of them is really the point.
//!
//! `index` builds an index out of a directory so there is something to ask
//! questions of. `search` runs a query and prints what came back. `explain`
//! runs the same query and prints what the engine did to answer it.
//!
//! `explain` exists because a timing on its own is not a diagnosis. A term in
//! four hundred thousand documents costing twenty nine milliseconds is equally
//! consistent with the pruning never firing and with the pruning working
//! perfectly and the time going somewhere else, and those need opposite fixes.
//! The counters that separate them live in `kura_core::explain`, and this is
//! how a person points them at a query.
//!
//! There are no dependencies here for the same reason there are none in the
//! engine. Argument parsing is forty lines and a crate is forever.

mod report;

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use kura_core::analysis::Analyzer;
use kura_core::index::{Reader, Writer};
use kura_core::search::Searcher;
use kura_core::segment::Segment;
use kura_core::store::Scratch;

/// How many results a command prints when nobody says otherwise.
const DEFAULT_HITS: usize = 10;

/// The field an indexed file carries so a hit can be traced back to it.
const PATH_FIELD: &str = "path";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("kura: {failure}");
            if let Failure::Usage(_) = failure {
                eprintln!();
                eprintln!("{USAGE}");
            }
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage:
  kura index <path>... -o <index>   index files and directories into <index>
  kura search <index> <query>       print the best matches
  kura explain <index> <query>      print what the query did to find them

options:
  -k <n>        how many results, for search and explain (default 10)
  -o <file>     where to write, for index
  --total       for explain, walk for the total as well as the page";

fn run() -> Result<(), Failure> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or_else(|| Failure::usage("no command"))?;
    let rest: Vec<String> = args.collect();
    match command.as_str() {
        "index" => index(&rest),
        "search" => query(&rest, false),
        "explain" => query(&rest, true),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(Failure::usage(format!("unknown command {other}"))),
    }
}

/// Builds an index out of whatever the paths point at.
fn index(args: &[String]) -> Result<(), Failure> {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "-o" => {
                at += 1;
                let path = args
                    .get(at)
                    .ok_or_else(|| Failure::usage("-o wants a file"))?;
                out = Some(PathBuf::from(path));
            }
            other => inputs.push(PathBuf::from(other)),
        }
        at += 1;
    }
    if inputs.is_empty() {
        return Err(Failure::usage("nothing to index"));
    }
    let out = out.ok_or_else(|| Failure::usage("no -o, so nowhere to write"))?;

    let mut files = Vec::new();
    for input in &inputs {
        collect(input, &mut files)?;
    }
    files.sort();

    let started = Instant::now();
    let mut writer = Writer::new();
    let mut bytes = 0u64;
    let mut skipped = 0usize;
    for file in &files {
        let Ok(content) = fs::read(file) else {
            skipped += 1;
            continue;
        };
        // A directory of anything real holds files that are not text, and a
        // lossy decode indexes the words in a mixed file rather than dropping
        // it. What it must not do is silently index a megabyte of replacement
        // characters, which is what a binary would become.
        let text = String::from_utf8_lossy(&content);
        if looks_binary(&text) {
            skipped += 1;
            continue;
        }
        bytes += content.len() as u64;
        let path = file.to_string_lossy().into_owned();
        writer.add_with_fields(&text, [(PATH_FIELD, path.as_bytes())])?;
    }
    let documents = writer.len();
    let segment = writer.finish()?;
    fs::write(&out, &segment).map_err(|error| Failure::Io(out.clone(), error))?;
    let took = started.elapsed();

    println!(
        "indexed {documents} documents, {} of text into {}, {} in {:.1?}",
        report::bytes(bytes),
        out.display(),
        report::bytes(segment.len() as u64),
        took
    );
    if skipped > 0 {
        println!("skipped {skipped} files that were not text");
    }
    Ok(())
}

/// Runs a query, and says what it did when `explaining`.
fn query(args: &[String], explaining: bool) -> Result<(), Failure> {
    let mut positional: Vec<&str> = Vec::new();
    let mut k = DEFAULT_HITS;
    let mut with_total = false;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "-k" => {
                at += 1;
                let value = args
                    .get(at)
                    .ok_or_else(|| Failure::usage("-k wants a number"))?;
                k = value
                    .parse()
                    .map_err(|_| Failure::usage(format!("-k wants a number, got {value}")))?;
            }
            "--total" => with_total = true,
            other => positional.push(other),
        }
        at += 1;
    }
    let [path, terms @ ..] = positional.as_slice() else {
        return Err(Failure::usage("no index file"));
    };
    if terms.is_empty() {
        return Err(Failure::usage("no query"));
    }
    // Everything after the index file is the query, so a shell that split it on
    // spaces and a shell that quoted it give the same answer.
    let text = terms.join(" ");

    let path = Path::new(path);
    let bytes = fs::read(path).map_err(|error| Failure::Io(path.to_path_buf(), error))?;
    let segment = Segment::open(&bytes)?;
    let index = Reader::open(&segment)?;
    let searcher = Searcher::new(&index);

    // Which walk is being explained matters more than it looks. Asking for the
    // total as well as the page means every matching document has to be visited
    // to be counted, so there is nothing for the pruning to skip and the skip
    // counters read zero on a query where the pruning is working perfectly. The
    // default here is therefore the page walk, which is the one the pruning
    // applies to, and `--total` explains the other one on purpose.
    let started = Instant::now();
    let (hits, total, counters) = match (explaining, with_total) {
        (true, false) => {
            let (hits, counters) = searcher.search_explained(&text, k)?;
            (hits, None, counters)
        }
        (true, true) => {
            let (hits, total, counters) = searcher.search_and_count_explained(&text, k)?;
            (hits, Some(total), counters)
        }
        (false, _) => {
            let (hits, total) = searcher.search_and_count(&text, k)?;
            (hits, Some(total), kura_core::explain::Counters::default())
        }
    };
    let took = started.elapsed();

    if explaining {
        let walk = if with_total {
            report::Walk::PageAndTotal
        } else {
            report::Walk::Page
        };
        report::plan(&text, &index, &mut std::io::stdout()).map_err(Failure::Stdout)?;
        report::counters(&counters, took, walk, &mut std::io::stdout()).map_err(Failure::Stdout)?;
    }

    match total {
        Some(total) => println!("{total} matching, showing {}", hits.len()),
        None => println!("showing {}", hits.len()),
    }
    let mut scratch = Scratch::new();
    for (rank, hit) in hits.iter().enumerate() {
        let label = match index.store() {
            Some(store) => match store.get(hit.doc, &mut scratch)?.field(PATH_FIELD)? {
                Some(path) => String::from_utf8_lossy(path).into_owned(),
                None => format!("doc {}", hit.doc),
            },
            None => format!("doc {}", hit.doc),
        };
        println!("{:>3}  {:>8.4}  {label}", rank + 1, hit.score);
    }
    Ok(())
}

/// Adds `path` to `files`, walking it when it is a directory.
///
/// Iterative rather than recursive because a directory tree is an input and a
/// deep enough one would take the stack with it.
fn collect(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), Failure> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        let meta = fs::metadata(&next).map_err(|error| Failure::Io(next.clone(), error))?;
        if meta.is_file() {
            files.push(next);
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        let entries = fs::read_dir(&next).map_err(|error| Failure::Io(next.clone(), error))?;
        for entry in entries {
            let entry = entry.map_err(|error| Failure::Io(next.clone(), error))?;
            let name = entry.file_name();
            // A repository is mostly its own history and a build directory is
            // mostly its own output, and indexing either says more about the
            // tool than about the corpus.
            if name == ".git" || name == "target" || name == "node_modules" {
                continue;
            }
            pending.push(entry.path());
        }
    }
    Ok(())
}

/// Whether a lossy decode turned enough of the input into replacement
/// characters that indexing it would be indexing noise.
///
/// A tenth is well above what a text file with one bad byte produces and well
/// below what a binary produces, so the exact figure does not matter much.
fn looks_binary(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let replaced = text
        .chars()
        .filter(|c| *c == char::REPLACEMENT_CHARACTER)
        .count();
    replaced * 10 > text.chars().count()
}

/// Analyses a query into its distinct terms, in order.
///
/// The same thing [`Searcher`] does internally, done again here because the
/// report names the terms and a report that named different terms from the ones
/// the walk opened would be worse than no report.
fn analyse(query: &str) -> Vec<Vec<u8>> {
    let mut analyzer = Analyzer::new();
    let mut words: Vec<Vec<u8>> = Vec::new();
    analyzer.analyze(query, |term, _| words.push(term.to_vec()));
    words.sort_unstable();
    words.dedup();
    words
}

/// What went wrong, with enough context to act on.
#[derive(Debug)]
enum Failure {
    /// The command line did not make sense.
    Usage(String),
    /// A file could not be read or written, and which one.
    Io(PathBuf, std::io::Error),
    /// Writing the report failed, which in practice is a closed pipe.
    Stdout(std::io::Error),
    /// The engine refused the data.
    Engine(kura_core::Error),
}

impl Failure {
    fn usage(what: impl Into<String>) -> Self {
        Self::Usage(what.into())
    }
}

impl From<kura_core::Error> for Failure {
    fn from(error: kura_core::Error) -> Self {
        Self::Engine(error)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(what) => write!(f, "{what}"),
            Self::Io(path, error) => write!(f, "{}: {error}", path.display()),
            Self::Stdout(error) => write!(f, "writing the report: {error}"),
            Self::Engine(error) => write!(f, "{error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_file_with_one_bad_byte_is_still_text() {
        let bytes = b"the quick brown fox\xff jumps over the lazy dog";
        let text = String::from_utf8_lossy(bytes);
        assert!(!looks_binary(&text));
    }

    #[test]
    fn a_run_of_undecodable_bytes_is_not_text() {
        let bytes = vec![0xff_u8; 512];
        let text = String::from_utf8_lossy(&bytes);
        assert!(looks_binary(&text));
    }

    #[test]
    fn an_empty_file_is_not_binary() {
        assert!(!looks_binary(""));
    }
}
