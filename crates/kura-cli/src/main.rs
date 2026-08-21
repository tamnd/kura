//! Command line access to a kura index.
//!
//! Five commands, in two groups.
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
//! `topics` and `eval` are the other half of the same argument. `explain` says
//! what a query cost, and those two say whether the answer was any good.
//! `topics` runs a file of queries and writes a run file, `eval` scores a run
//! file against judgments, and between them a ranking change stops being a
//! matter of opinion. See [`eval`] for what the numbers mean.
//!
//! There are no dependencies here for the same reason there are none in the
//! engine. Argument parsing is forty lines and a crate is forever.

mod eval;
mod map;
mod report;

use std::fmt;
use std::fs;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use kura_core::analysis::Analyzer;
use kura_core::index::{Reader, Writer};
use kura_core::residency;
use kura_core::search::Searcher;
use kura_core::segment::Segment;
use kura_core::store::Scratch;

use crate::map::Map;

/// How many results a command prints when nobody says otherwise.
const DEFAULT_HITS: usize = 10;

/// How deep a run file goes when nobody says otherwise.
///
/// A thousand is the depth TREC pools to and the depth every published run is
/// written at, and recall at a hundred needs a hundred of them to be there.
const DEFAULT_DEPTH: usize = 1_000;

/// The field an indexed file carries so a hit can be traced back to it.
const PATH_FIELD: &str = "path";

/// What a run file calls a run that nobody named.
const DEFAULT_TAG: &str = "kura";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("kura-cli: {failure}");
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
  kura-cli index <path>... -o <index>        index files and directories into <index>
  kura-cli search <index> <query>            print the best matches
  kura-cli explain <index> <query>           print what the query did to find them
  kura-cli topics <index> <topics> -o <run>  answer a file of queries into a run file
  kura-cli eval <qrels> <run>                score a run file against judgments

options:
  -k <n>        how many results, for search and explain (default 10)
  -o <file>     where to write, for index and topics
  --total       for explain, walk for the total as well as the page
  --depth <n>   how deep a run file goes, for topics (default 1000)
  --tag <name>  what the run file calls this run (default kura)
  --field <f>   which stored field names a document (default path)
  --verify      check the index checksum before querying, which reads all of it
  --complete    for eval, score every judged query and not only the answered ones
  --per-query   for eval, print a line per query as well as the averages

formats:
  topics    one query per line, the identifier and the text separated by a tab
  qrels     query, iteration, document, relevance
  run       query, iteration, document, rank, score, tag";

fn run() -> Result<(), Failure> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or_else(|| Failure::usage("no command"))?;
    let rest: Vec<String> = args.collect();
    match command.as_str() {
        "index" => index(&rest),
        "search" => query(&rest, false),
        "explain" => query(&rest, true),
        "topics" => topics(&rest),
        "eval" => evaluate(&rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(Failure::usage(format!("unknown command {other}"))),
    }
}

/// Answers every query in a topics file and writes a run file.
///
/// The half of a relevance measurement that needs the engine. The other half is
/// [`evaluate`], which needs only the run file and the judgments, so the two are
/// separate commands and a run written here can be scored by `trec_eval` and a
/// run written by another engine can be scored here.
fn topics(args: &[String]) -> Result<(), Failure> {
    let mut positional: Vec<&str> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut depth = DEFAULT_DEPTH;
    let mut tag = DEFAULT_TAG.to_string();
    let mut field = PATH_FIELD.to_string();
    let mut verify = false;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "--verify" => verify = true,
            "-o" => {
                at += 1;
                out = Some(PathBuf::from(want(args, at, "-o wants a file")?));
            }
            "--depth" => {
                at += 1;
                let value = want(args, at, "--depth wants a number")?;
                depth = value
                    .parse()
                    .map_err(|_| Failure::usage(format!("--depth wants a number, got {value}")))?;
            }
            "--tag" => {
                at += 1;
                tag = want(args, at, "--tag wants a name")?.to_string();
            }
            "--field" => {
                at += 1;
                field = want(args, at, "--field wants a name")?.to_string();
            }
            other => positional.push(other),
        }
        at += 1;
    }
    let [index, queries] = positional.as_slice() else {
        return Err(Failure::usage("wanted an index and a topics file"));
    };
    let out = out.ok_or_else(|| Failure::usage("no -o, so nowhere to write the run"))?;

    let index = Path::new(index);
    let bytes = Map::open(index).map_err(|error| Failure::Io(index.to_path_buf(), error))?;
    let segment = open(&bytes, verify)?;
    let reader = Reader::open(&segment)?;
    let searcher = Searcher::new(&reader);

    let queries = Path::new(queries);
    let text = fs::read_to_string(queries).map_err(|e| Failure::Io(queries.to_path_buf(), e))?;

    let file = fs::File::create(&out).map_err(|error| Failure::Io(out.clone(), error))?;
    let mut writer = BufWriter::new(file);
    let mut scratch = Scratch::new();
    let started = Instant::now();
    let mut answered = 0usize;
    let mut lines = 0u64;

    for (at, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A tab if there is one, and the first run of whitespace if there is
        // not, so a file written with spaces is not silently read as a query
        // with no identifier.
        let (id, query) = line
            .split_once('\t')
            .or_else(|| line.split_once(char::is_whitespace))
            .ok_or_else(|| {
                Failure::usage(format!(
                    "{}: line {} has an identifier and no query",
                    queries.display(),
                    at + 1
                ))
            })?;
        let query = query.trim();
        let hits = searcher.search(query, depth)?;
        answered += 1;
        for (rank, hit) in hits.iter().enumerate() {
            let doc = label(&reader, hit.doc, &field, &mut scratch)?;
            writeln!(
                writer,
                "{id}\tQ0\t{doc}\t{}\t{:.6}\t{tag}",
                rank + 1,
                hit.score
            )
            .map_err(|error| Failure::Io(out.clone(), error))?;
            lines += 1;
        }
    }
    writer
        .flush()
        .map_err(|error| Failure::Io(out.clone(), error))?;
    let took = started.elapsed();

    println!(
        "answered {answered} queries into {}, {lines} lines in {:.1?}",
        out.display(),
        took
    );
    if answered > 0 {
        println!(
            "{:.3?} per query",
            took / u32::try_from(answered).unwrap_or(1)
        );
    }
    Ok(())
}

/// Scores a run file against judgments.
fn evaluate(args: &[String]) -> Result<(), Failure> {
    let mut positional: Vec<&str> = Vec::new();
    let mut coverage = eval::Coverage::Answered;
    let mut per_query = false;
    for arg in args {
        match arg.as_str() {
            "--complete" => coverage = eval::Coverage::Complete,
            "--per-query" => per_query = true,
            other => positional.push(other),
        }
    }
    let [qrels, run] = positional.as_slice() else {
        return Err(Failure::usage("wanted a qrels file and a run file"));
    };

    let qrels_path = Path::new(qrels);
    let qrels_text =
        fs::read_to_string(qrels_path).map_err(|e| Failure::Io(qrels_path.to_path_buf(), e))?;
    let qrels = eval::Qrels::parse(&qrels_text)
        .map_err(|bad| Failure::Format(qrels_path.to_path_buf(), bad))?;

    let run_path = Path::new(run);
    let run_text =
        fs::read_to_string(run_path).map_err(|e| Failure::Io(run_path.to_path_buf(), e))?;
    let run =
        eval::Run::parse(&run_text).map_err(|bad| Failure::Format(run_path.to_path_buf(), bad))?;

    // An empty file scores zero on every measure, which reads exactly like a
    // ranking that found nothing, so say which one it was.
    if qrels.is_empty() {
        return Err(Failure::Empty(
            qrels_path.to_path_buf(),
            "no judgments, so there is nothing to score against",
        ));
    }
    if run.is_empty() {
        return Err(Failure::Empty(
            run_path.to_path_buf(),
            "no results, so every measure would be zero",
        ));
    }

    let scores = eval::score(&run, &qrels, coverage);
    println!("judged   {} queries", qrels.len());
    println!("answered {} queries", run.len());
    println!("scored   {} queries", scores.queries);
    println!();

    if per_query {
        println!(
            "  {:<20} {:>10} {:>10} {:>10}",
            "query", "ndcg@10", "recall@100", "mrr@10"
        );
        for (query, one) in eval::each(&run, &qrels, coverage) {
            println!(
                "  {query:<20} {:>10.4} {:>10.4} {:>10.4}",
                one.ndcg_10, one.recall_100, one.mrr_10
            );
        }
        println!();
    }

    // Named the way trec_eval names them, so a number from here and a number
    // from there can be put in the same table without a footnote.
    println!("  {:<16} {:.4}", "ndcg_cut_10", scores.ndcg_10);
    println!("  {:<16} {:.4}", "recall_100", scores.recall_100);
    println!("  {:<16} {:.4}", "recip_rank_10", scores.mrr_10);
    Ok(())
}

/// The argument at `at`, or a usage failure saying what was wanted.
fn want<'a>(args: &'a [String], at: usize, wanted: &str) -> Result<&'a str, Failure> {
    args.get(at)
        .map(String::as_str)
        .ok_or_else(|| Failure::usage(wanted.to_string()))
}

/// What to call a document in output a person or another tool will read.
fn label(
    index: &Reader<'_>,
    doc: kura_core::DocId,
    field: &str,
    scratch: &mut Scratch,
) -> Result<String, Failure> {
    match index.store() {
        Some(store) => match store.get(doc, scratch)?.field(field)? {
            Some(value) => Ok(String::from_utf8_lossy(value).into_owned()),
            None => Ok(doc.to_string()),
        },
        None => Ok(doc.to_string()),
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

/// Opens a mapped index, checking the checksum only when asked to.
///
/// The structural checks happen either way, so a section this hands back is
/// inside the file whichever branch ran. What `verify` adds is a read of every
/// byte to confirm that the contents are the contents that were written.
///
/// It is off by default, and that is a decision rather than an oversight.
/// Verifying is linear in the size of the index, so on a mapped file it faults
/// in the whole thing before the query starts, which is exactly the copy that
/// mapping exists to avoid. On a 33.2 MB index that is the difference between
/// touching a few hundred kilobytes and touching all of it, and the ratio only
/// gets worse as an index grows, because a query touches the dictionary, one
/// skip table per term and the blocks it does not step over.
///
/// A whole file check on every open is also the wrong shape for the job. It
/// answers "was this file ever damaged" at a cost paid by every query, when the
/// question a query needs answered is "is the block I am about to decode
/// intact". Per section checksums are on the plan for the same reason, and when
/// they land this stops being a trade and `--verify` goes back to being what it
/// says: an explicit check of a file you have a reason to doubt.
fn open(bytes: &[u8], verify: bool) -> Result<Segment<'_>, Failure> {
    if verify {
        Ok(Segment::open(bytes)?)
    } else {
        Ok(Segment::open_without_checksum(bytes)?)
    }
}

/// Runs a query, and says what it did when `explaining`.
fn query(args: &[String], explaining: bool) -> Result<(), Failure> {
    let mut positional: Vec<&str> = Vec::new();
    let mut k = DEFAULT_HITS;
    let mut with_total = false;
    let mut verify = false;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "--verify" => verify = true,
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
    // Mapped rather than read. The whole point of `explain` is to say what a
    // query cost, and reading the index first charges the query for a copy of
    // an index it will touch a fraction of. See [`map`].
    let bytes = Map::open(path).map_err(|error| Failure::Io(path.to_path_buf(), error))?;
    let segment = open(&bytes, verify)?;
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
        // Wrapped in a probe, so the report can say how much of the index was
        // already in memory and how much of it this query had to fetch. Only
        // `explain` pays for it, and only `explain` prints it.
        (true, false) => {
            let ((hits, total), counters) = residency::measured(&bytes, || {
                let (hits, counters) = searcher.search_explained(&text, k)?;
                Ok::<_, Failure>(((hits, None), counters))
            })?;
            (hits, total, counters)
        }
        (true, true) => {
            let ((hits, total), counters) = residency::measured(&bytes, || {
                let (hits, total, counters) = searcher.search_and_count_explained(&text, k)?;
                Ok::<_, Failure>(((hits, Some(total)), counters))
            })?;
            (hits, total, counters)
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
    /// A qrels or run file did not parse, and which line of it.
    Format(PathBuf, eval::Bad),
    /// A file that parsed and held nothing.
    Empty(PathBuf, &'static str),
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
            Self::Format(path, bad) => write!(f, "{}: {bad}", path.display()),
            Self::Empty(path, why) => write!(f, "{}: {why}", path.display()),
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

    /// A small index, and the same index with one byte of its body flipped.
    fn an_index_and_a_damaged_copy() -> (Vec<u8>, Vec<u8>) {
        let mut writer = Writer::new();
        for text in ["the quick brown fox", "jumps over the lazy dog"] {
            writer.add(text).expect("two documents fit");
        }
        let good = writer.finish().expect("what was written decodes");

        let mut damaged = good.clone();
        // Past the header, so what is broken is a byte of content rather than
        // a byte of structure. A structural break is caught either way and
        // would not tell the two branches apart.
        let at = damaged.len() / 2;
        damaged[at] ^= 0x01;
        (good, damaged)
    }

    #[test]
    fn a_damaged_index_opens_without_verifying_and_does_not_with() {
        // The whole difference the flag makes, in one test. Without it the
        // structural checks run and the contents are taken on trust, which is
        // what makes opening independent of the size of the index. With it the
        // contents are read and the damage is found.
        let (good, damaged) = an_index_and_a_damaged_copy();

        assert!(open(&good, false).is_ok());
        assert!(open(&good, true).is_ok());
        assert!(open(&damaged, false).is_ok());
        assert!(open(&damaged, true).is_err());
    }

    #[test]
    fn a_file_that_is_not_an_index_is_refused_either_way() {
        // Otherwise the default would be no check at all rather than a
        // structural one, and the first thing anybody points this at by mistake
        // is a file that is not an index.
        let rubbish = vec![0x5a_u8; 4_096];
        assert!(open(&rubbish, false).is_err());
        assert!(open(&rubbish, true).is_err());
    }
}
