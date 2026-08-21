//! Command line access to a kura index.
//!
//! Nine commands, in three groups.
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
//! `verify`, `dump`, `repair` and `migrate` are the third group, and they are
//! for the file rather than for the answers. `verify` reads an index all the way
//! through and says whether it is intact. `dump` prints what is in it, one
//! record to a line, for the questions that start with somebody not believing
//! what came back. `repair` is what comes after a `verify` that failed, and it
//! does the one repair a store supports, which is committing a manifest that
//! leaves out the segments that no longer read. `migrate` reads a file written
//! by an older build and writes today's format beside it.
//!
//! There are no dependencies here for the same reason there are none in the
//! engine. Argument parsing is forty lines and a crate is forever.

mod dump;
mod eval;
mod migrate;
mod repair;
mod report;
mod verify;

use std::fmt;
use std::fs;
use std::io::{BufWriter, Write as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use kura_core::analysis::Analyzer;
use kura_core::file::{Store, Trouble};
use kura_core::index::{Held, Reader, Writer};
use kura_core::manifest;
use kura_core::mapping::Map;
use kura_core::residency;
use kura_core::search::Searcher;
use kura_core::segment::{Segment, Writer as SegmentWriter};
use kura_core::store::Scratch;

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

/// How long the log ring is in a store this tool makes.
///
/// Well under the engine's default quarter of a gigabyte, because this tool
/// writes a segment straight into the store, so it never writes a record and the
/// ring is there for whatever opens the store next rather than for anything
/// happening here. The size is fixed when the store is made and
/// cannot be changed afterwards without moving every segment in the file, so it
/// is not nothing, but eight megabytes is thousands of records and a store that
/// wants more than that is a store a program made rather than one made here.
///
/// It also keeps the file small on a filesystem that reserves the space a file
/// says it is long rather than handing it out as it is written to.
const LOG_LEN: u64 = 8 << 20;

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
  kura-cli verify <index>                    read an index through and report what is wrong
  kura-cli dump <index>                      print what is in an index, one record to a line
  kura-cli repair <store>                    drop the segments that no longer read
  kura-cli migrate <index> -o <new>          write an older index out in today's format

options:
  -k <n>        how many results, for search and explain (default 10)
  -o <file>     where to write, for index, topics and migrate
  --store       for index, add a segment to a store rather than write a bare one
  --flush-every <size>  for index, start a new segment once this much text has gone in
  --total       for explain, walk for the total as well as the page
  --depth <n>   how deep a run file goes, for topics (default 1000)
  --tag <name>  what the run file calls this run (default kura)
  --field <f>   which stored field names a document (default path)
  --verify      check the index checksum before querying, which reads all of it
  --complete    for eval, score every judged query and not only the answered ones
  --per-query   for eval, print a line per query as well as the averages
  --postings    for dump, a line per posting rather than a line per term
  --documents   for dump, a line per stored field rather than a line per term
  --term <t>    for dump, only this term, which reads no others
  --limit <n>   for dump, stop after this many records
  --commit      for repair, write the manifest rather than only say what it would be

an <index> is either a single segment, which is what index writes by default,
or a store holding any number of them, which is what --store writes into. Every
command that reads one takes either, and a query over a store searches all of
its segments together.

--flush-every takes a plain number of bytes or a number with k, m or g after it,
and it is what bounds how much of a corpus an index run holds at once. Without
it a run keeps every posting in memory until the last file has been read, so the
memory it needs is the size of what it was pointed at.

a dump is tab separated with a comment line naming its columns, and every line
says which segment it came from. It prints terms and stored fields, which is to
say it prints the corpus, so treat what comes out of it the way the corpus it
came from has to be treated.

a migrate never writes in place and never writes over a file that is there. It
reads one index and writes another, and what to do with the original is left to
whoever ran it. An index that is already in today's format is left alone and
nothing is written.

a repair prints what it would do and writes nothing until it is given --commit,
because what it does is throw documents away. It never touches a segment, only
the manifest that names them, and the manifest it replaces stays in the store
until the commit after it.

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
        "verify" => check(&rest),
        "dump" => show(&rest),
        "repair" => mend(&rest),
        "migrate" => forward(&rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(Failure::usage(format!("unknown command {other}"))),
    }
}

/// Prints what is inside an index, one record to a line.
///
/// See [`dump`] for the shape of the output and for what it means that this is
/// the one command here which prints the corpus back out.
fn show(args: &[String]) -> Result<(), Failure> {
    let mut positional = Vec::new();
    let mut what = dump::What::Terms;
    let mut chosen: Option<&str> = None;
    let mut term: Option<Vec<u8>> = None;
    let mut limit: Option<u64> = None;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            // Two modes at once is a typo rather than a request, and the two
            // answers it could be given, the first flag or the last, are both
            // wrong half the time.
            flag @ ("--postings" | "--documents") => {
                if let Some(already) = chosen {
                    return Err(Failure::usage(format!("{already} and {flag} at once")));
                }
                chosen = Some(flag);
                what = if flag == "--postings" {
                    dump::What::Postings
                } else {
                    dump::What::Documents
                };
            }
            "--term" => {
                at += 1;
                term = Some(want(args, at, "--term wants a term")?.as_bytes().to_vec());
            }
            "--limit" => {
                at += 1;
                let value = want(args, at, "--limit wants a number")?;
                limit =
                    Some(value.parse().map_err(|_| {
                        Failure::usage(format!("--limit wants a number, got {value}"))
                    })?);
            }
            other => positional.push(other),
        }
        at += 1;
    }
    let [path] = positional.as_slice() else {
        return Err(Failure::usage("wanted one index file"));
    };

    // A term on its own means the postings of that term, because a person who
    // names a term wants to see its list and the one line the dictionary holds
    // about it is already in the terms mode they did not ask for.
    if term.is_some() && chosen.is_none() {
        what = dump::What::Postings;
    }

    let path = Path::new(path);
    let bytes = Map::open(path).map_err(|error| Failure::Io(path.to_path_buf(), error))?;
    let segments = segments_in(&bytes, false)?;
    let readers = readers_of(&segments)?;

    let request = dump::Request {
        what,
        term: term.as_deref(),
        limit,
    };
    let mut out = BufWriter::new(std::io::stdout());
    dump::to_stdout(&readers, request, &mut out)?;
    match out.flush() {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Failure::Stdout),
    }
}

/// Reads an index all the way through and says whether it is intact.
///
/// The report goes to standard output whatever it says, because a person who
/// ran this wants to see it either way, and a damaged index is a failing exit
/// rather than an error, because the report has already said everything there
/// is to say about it. See [`verify`] for what is checked and what is not.
fn check(args: &[String]) -> Result<(), Failure> {
    let [path] = args else {
        return Err(Failure::usage("wanted one index file"));
    };
    let path = Path::new(path);

    let mut out = BufWriter::new(std::io::stdout());
    let outcome = verify::check(path, &mut out).map_err(|error| {
        // Reading the index and writing the report both come back as an io
        // error here, and only one of them has a path worth printing. The
        // report goes through a buffer, so a failure writing it surfaces at the
        // flush below rather than out of here.
        Failure::Io(path.to_path_buf(), error)
    })?;
    out.flush().map_err(Failure::Stdout)?;

    if outcome.failures > 0 {
        return Err(Failure::Damaged(path.to_path_buf(), outcome.failures));
    }
    Ok(())
}

/// Drops the segments of a store that no longer read.
///
/// The exit code answers one question, which is whether the store is readable
/// as it stands. A store with nothing wrong with it and a store that has just
/// had its damage committed away both succeed. A dry run over a damaged store
/// fails, because the report is accurate and nothing was fixed, and a script
/// that ran this without `--commit` should not carry on as though it had. See
/// [`repair`] for what the one repair is and why there is only one.
fn mend(args: &[String]) -> Result<(), Failure> {
    let mut positional = Vec::new();
    let mut commit = false;
    for arg in args {
        match arg.as_str() {
            "--commit" => commit = true,
            other if other.starts_with("--") => {
                return Err(Failure::usage(format!("unknown option {other}")));
            }
            other => positional.push(other),
        }
    }
    let [path] = positional[..] else {
        return Err(Failure::usage("wanted one store file"));
    };
    let path = Path::new(path);

    let mut out = BufWriter::new(std::io::stdout());
    let outcome = repair::repair(path, commit, now(), &mut out)
        .map_err(|trouble| Failure::Store(path.to_path_buf(), trouble))?;
    out.flush().map_err(Failure::Stdout)?;

    if outcome.settled() {
        return Ok(());
    }
    Err(Failure::Damaged(path.to_path_buf(), outcome.damaged))
}

/// Writes an index that an older build wrote out in today's format.
///
/// The exit code answers one question, which is whether there is now a file this
/// build will open. An index that was migrated and an index that needed no
/// migration both succeed, and a refusal fails, because a refusal means the file
/// is still one this build will not read and a script should not carry on as
/// though it had been fixed. See [`migrate`] for what it refuses and why.
fn forward(args: &[String]) -> Result<(), Failure> {
    let mut positional = Vec::new();
    let mut into: Option<PathBuf> = None;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "-o" => {
                at += 1;
                into = Some(PathBuf::from(want(args, at, "-o wants a file")?));
            }
            other if other.starts_with("--") => {
                return Err(Failure::usage(format!("unknown option {other}")));
            }
            other => positional.push(other),
        }
        at += 1;
    }
    let [path] = positional[..] else {
        return Err(Failure::usage("wanted one index file"));
    };
    let path = Path::new(path);
    let into = into.ok_or_else(|| Failure::usage("migrate wants -o, and never writes in place"))?;

    let mut out = BufWriter::new(std::io::stdout());
    let outcome = migrate::migrate(path, &into, now(), &mut out)
        .map_err(|trouble| Failure::Store(path.to_path_buf(), trouble))?;
    out.flush().map_err(Failure::Stdout)?;

    if outcome.settled() {
        return Ok(());
    }
    Err(Failure::Refused(path.to_path_buf()))
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
    let segments = segments_in(&bytes, verify)?;
    let readers = readers_of(&segments)?;
    let searcher = Searcher::over(&readers)?;

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
            // A run file is read by another program, so an unnamed document is
            // its number and nothing else. The word "doc" in front of it would
            // be part of the identifier as far as a scorer is concerned.
            let doc = label(&searcher, hit.doc, &field, &mut scratch)?
                .unwrap_or_else(|| hit.doc.to_string());
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

/// What a document is called, according to a stored field of it.
///
/// The identifier goes through the searcher first because hits are numbered
/// across every segment and stored fields are not. A segment holds its own
/// documents under its own numbering and knows nothing about the segments before
/// it, so asking one of them for document forty thousand when the number came
/// out of a search over eight of them gets an answer, and the answer is about
/// some other document.
///
/// Returns nothing when the document has no such field, or none at all, which is
/// a different situation from a lookup that failed and is left to the caller to
/// word.
fn label(
    searcher: &Searcher<'_, '_>,
    doc: kura_core::DocId,
    field: &str,
    scratch: &mut Scratch,
) -> Result<Option<String>, Failure> {
    let Some((at, local)) = searcher.locate(doc) else {
        return Ok(None);
    };
    let Some(index) = searcher.segments().get(at) else {
        return Ok(None);
    };
    let Some(store) = index.store() else {
        return Ok(None);
    };
    match store.get(local, scratch)?.field(field)? {
        Some(value) => Ok(Some(String::from_utf8_lossy(value).into_owned())),
        None => Ok(None),
    }
}

/// The high water mark read at three points in an index run.
///
/// A high water mark never falls, so these are cumulative rather than separate,
/// and each one says how much further the work between it and the one before it
/// pushed the worst the process had been. That is what separates the memory a
/// writer accumulates from the memory the finish needs to turn it into a segment
/// from the pages of the file that was just written.
///
/// A platform that will not answer leaves them all as errors and the run says
/// nothing rather than printing zeroes.
#[derive(Default)]
struct Marks {
    documents: Option<Result<u64, &'static str>>,
    merged: Option<Result<u64, &'static str>>,
    written: Option<Result<u64, &'static str>>,
}

impl Marks {
    /// The three readings, or nothing if this system does not keep them.
    fn read(&self) -> Option<[u64; 3]> {
        match (&self.documents, &self.merged, &self.written) {
            (Some(Ok(a)), Some(Ok(b)), Some(Ok(c))) => Some([*a, *b, *c]),
            _ => None,
        }
    }

    /// Prints them, or prints nothing on a system that does not keep them.
    ///
    /// The first number is the whole of it and the three after it are what each
    /// step added, because a reader who wants to know where to look next wants
    /// the steps and a reader who wants to know whether this run fits on the
    /// machine wants the total.
    fn tell(&self) {
        let Some([documents, merged, written]) = self.read() else {
            return;
        };
        println!(
            "peak resident {}, of which {} by the last document, {} more merging the postings, {} more writing the segment",
            report::bytes(written),
            report::bytes(documents),
            report::bytes(merged - documents),
            report::bytes(written - merged)
        );
    }
}

/// Prints what a writer was holding at its largest.
///
/// Split rather than totalled, because the total on its own says a run needs so
/// much more memory than the text takes and stops there, and the parts are what
/// say which of them to go after. The number is the largest a single writer got,
/// so on a run with --flush-every it is the peak of one segment and not of the
/// corpus.
fn tell_held(peak: Held) {
    println!(
        "held at most {} at once, {} postings, {} vocabulary, {} stored fields, {} lengths",
        report::bytes(peak.total()),
        report::bytes(peak.postings),
        report::bytes(peak.vocabulary),
        report::bytes(peak.stored),
        report::bytes(peak.lengths)
    );
}

/// What an index run was asked to do, once its arguments have been read.
struct Plan {
    inputs: Vec<PathBuf>,
    out: PathBuf,
    into_store: bool,
    flush_every: Option<u64>,
}

impl Plan {
    /// Reads the arguments, or says which one does not make sense.
    fn read(args: &[String]) -> Result<Self, Failure> {
        let mut inputs: Vec<PathBuf> = Vec::new();
        let mut out: Option<PathBuf> = None;
        let mut into_store = false;
        let mut flush_every: Option<u64> = None;
        let mut at = 0;
        while at < args.len() {
            match args[at].as_str() {
                "--store" => into_store = true,
                "--flush-every" => {
                    at += 1;
                    let value = args
                        .get(at)
                        .ok_or_else(|| Failure::usage("--flush-every wants a size"))?;
                    flush_every = Some(size(value)?);
                }
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
        if flush_every.is_some() && !into_store {
            // A file that is one segment can hold one segment, and the whole of
            // what this option does is write more than one.
            return Err(Failure::usage("--flush-every needs --store to flush into"));
        }
        Ok(Self {
            inputs,
            out,
            into_store,
            flush_every,
        })
    }
}

/// Builds an index out of whatever the paths point at.
fn index(args: &[String]) -> Result<(), Failure> {
    let Plan {
        inputs,
        out,
        into_store,
        flush_every,
    } = Plan::read(args)?;

    let mut files = Vec::new();
    for input in &inputs {
        collect(input, &mut files)?;
    }
    files.sort();

    let started = Instant::now();
    let mut writer = Writer::new();
    let mut bytes = 0u64;
    let mut written = 0u64;
    let mut pending = 0u64;
    let mut documents = 0usize;
    let mut skipped = 0usize;
    let mut segments = 0usize;
    let mut peak = Held::default();
    let mut marks = Marks::default();
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
        pending += content.len() as u64;
        let path = file.to_string_lossy().into_owned();
        writer.add_with_fields(&text, [(PATH_FIELD, path.as_bytes())])?;
        // Asked after every document, because the largest a writer gets is the
        // document before the flush that empties it, and a reading taken once at
        // the end would be a reading of whatever the last part happened to be.
        let now = writer.held();
        if now.total() > peak.total() {
            peak = now;
        }

        // The budget is checked after the document rather than before it,
        // because a budget that stopped short of a document would be a budget
        // that refused documents larger than itself.
        if flush_every.is_some_and(|budget| pending >= budget) {
            let count = writer.len();
            let built = Writer::build(vec![std::mem::replace(&mut writer, Writer::new())])?;
            written += built.size() as u64;
            documents += count;
            segments = add_to_store(&out, built, count)?;
            pending = 0;
        }
    }

    // Three readings of the same high water mark, taken either side of the two
    // things that happen after the last document has been read. Nothing lowers a
    // high water mark, so what each of these says is how much further the one
    // before it was pushed, and that is the only way to tell the memory that
    // accumulates from the memory the merge needs from the pages of the file the
    // segment goes into.
    let read_documents = Some(residency::peak_resident());

    // The last flush, and on a run without the option the only one. A writer
    // holding nothing is not written out, because a segment of no documents is a
    // segment every later reader has to skip over for the rest of the file's
    // life.
    if !writer.is_empty() || documents == 0 {
        let count = writer.len();
        // Split where the engine splits it, so the reading between the two says
        // what the merge cost and what putting the segment where it goes cost
        // rather than what the pair of them came to.
        let built = Writer::build(vec![writer])?;
        let read_merged = Some(residency::peak_resident());
        written += built.size() as u64;
        documents += count;
        segments = if into_store {
            add_to_store(&out, built, count)?
        } else {
            write_bare(&out, built)?;
            1
        };
        marks = Marks {
            documents: read_documents,
            merged: read_merged,
            written: Some(residency::peak_resident()),
        };
    }
    let took = started.elapsed();

    println!(
        "indexed {documents} documents, {} of text into {}, {} in {:.1?}",
        report::bytes(bytes),
        out.display(),
        report::bytes(written),
        took
    );
    tell_held(peak);
    marks.tell();
    if into_store {
        println!("{} now holds {segments} segments", out.display());
    }
    if skipped > 0 {
        println!("skipped {skipped} files that were not text");
    }
    Ok(())
}

/// A size on the command line, in bytes or with a unit after it.
///
/// The units are powers of two, which is what everything else here prints, and
/// a run that has to be typed as 268435456 is a run somebody gets wrong once and
/// then does not repeat.
fn size(value: &str) -> Result<u64, Failure> {
    let (digits, scale) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1 << 10),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1 << 30),
        _ => (value, 1),
    };
    let count: u64 = digits
        .parse()
        .map_err(|_| Failure::usage(format!("{value} is not a size")))?;
    let bytes = count
        .checked_mul(scale)
        .ok_or_else(|| Failure::usage(format!("{value} is larger than this machine can count")))?;
    if bytes == 0 {
        return Err(Failure::usage(
            "a size of zero would flush after every document",
        ));
    }
    Ok(bytes)
}

/// Adds one segment to a store, making the store first if it is not there.
///
/// Returns how many segments the store holds afterwards.
///
/// Append then commit, which is the order the store insists on and the reason
/// this is safe to run twice over the same file. If the machine stops between
/// the two, the segment is bytes nobody names and the next run puts its own over
/// the top of them. What cannot happen is a store that names a segment which is
/// not there.
fn add_to_store(path: &Path, segment: SegmentWriter, documents: usize) -> Result<usize, Failure> {
    let now = now();
    let mut store = if path.exists() {
        Store::open(path)
    } else {
        Store::create_with_log(path, identity(path, now), now, LOG_LEN)
    }
    .map_err(|trouble| Failure::Store(path.to_path_buf(), trouble))?;

    let docs = u32::try_from(documents)
        .map_err(|_| Failure::usage("more documents than a single segment can number"))?;
    let mut manifest = store.manifest().clone();
    // Through the segment writer rather than through a vector of the finished
    // segment, so the bytes are laid out where they are going. Building the
    // vector first cost 20.6 MB on a corpus whose segment came to 14.3 MB, which
    // is a copy of the largest thing this program makes for the length of one
    // call.
    let written = store
        .append_segment_with(docs, now, |into| segment.write_to(into))
        .map_err(|trouble| Failure::Store(path.to_path_buf(), trouble))?;
    manifest.live = manifest.live.saturating_add(u64::from(docs));
    manifest.total = manifest.total.saturating_add(u64::from(docs));
    manifest.segments.push(written);
    let held = manifest.segments.len();
    store
        .commit(manifest, now)
        .map_err(|trouble| Failure::Store(path.to_path_buf(), trouble))?;
    Ok(held)
}

/// Writes one segment to a path of its own, with no store around it.
///
/// The same streaming the store gets, for the same reason. A file is not a store
/// and cannot be added to, so this replaces whatever was there rather than
/// appending to it, which is what `-o` on a run without `--store` has always
/// meant.
///
/// There is no buffer between the writer and the file. A segment goes out as a
/// header, a table, one write per section and a footer, and the sections are the
/// large part, so a buffer would only copy them again on the way past.
fn write_bare(path: &Path, segment: SegmentWriter) -> Result<(), Failure> {
    let mut file =
        fs::File::create(path).map_err(|error| Failure::Io(path.to_path_buf(), error))?;
    segment
        .write_to(&mut file)
        .map_err(|error| Failure::Io(path.to_path_buf(), error))
}

/// Now, in unix nanoseconds, or zero on a machine whose clock is before 1970.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// An identifier for a new store.
///
/// It exists so that a segment found loose somewhere can be tested for having
/// come from this store, which wants two stores made a moment apart on the same
/// machine to differ. The clock, the process and the path are enough for that.
/// It is not a random number and it is not trying to be one: nothing here rests
/// on it being unguessable.
fn identity(path: &Path, now: u64) -> u128 {
    let mut seed = Vec::with_capacity(64);
    seed.extend_from_slice(&now.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    seed.extend_from_slice(path.to_string_lossy().as_bytes());
    kura_core::xxh3::hash128(&seed)
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
/// intact". The format now checksums each section rather than the body, which
/// is half of the way there: a reader can check the dictionary without reading
/// the postings. A number per block is what would close it, and until that
/// lands this stays a trade rather than a free check.
fn open(bytes: &[u8], verify: bool) -> Result<Segment<'_>, Failure> {
    if verify {
        Ok(Segment::open(bytes)?)
    } else {
        Ok(Segment::open_without_checksum(bytes)?)
    }
}

/// Every segment in a mapped index file, whatever kind of file it is.
///
/// A store and a bare segment are told apart by the first eight bytes rather
/// than by trying one decoder and falling back to the other when it fails. The
/// fallback reads fine and behaves badly: a damaged store would fail the store
/// decode, get handed to the segment decode, fail that too, and report the
/// second failure, so a person with a torn manifest would be told their file is
/// not a segment. It is not, and that was never the question.
fn segments_in(bytes: &[u8], verify: bool) -> Result<Vec<Segment<'_>>, Failure> {
    Ok(parts_of(bytes, verify)?.0)
}

/// The segments, and the stretch of the file they sit in.
///
/// The second one is for the residency probe, which reports how much of the
/// index was already in memory and needs to know what counts as the index. In a
/// store that is the segment region and not the file: the log is a sparse
/// quarter of the file that a query never reads, and counting it in the
/// denominator turns a warm index into a cold looking percentage.
fn parts_of(bytes: &[u8], verify: bool) -> Result<(Vec<Segment<'_>>, Range<usize>), Failure> {
    if manifest::looks_like_a_store(bytes) {
        let (superblock, state) = manifest::front(bytes)?;
        let ranges = manifest::locate(&superblock, &state, bytes.len())?;
        let start = ranges.iter().map(|range| range.start).min().unwrap_or(0);
        let end = ranges.iter().map(|range| range.end).max().unwrap_or(0);
        let segments = ranges
            .into_iter()
            .map(|range| open(&bytes[range], verify))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok((segments, start..end));
    }
    Ok((vec![open(bytes, verify)?], 0..bytes.len()))
}

/// A reader per segment, in the order the segments were written.
///
/// Separate from [`segments_in`] because the borrow runs one way down the chain
/// and cannot be tied in a knot: the mapping owns the bytes, the segments borrow
/// the mapping, the readers borrow the segments, and the searcher borrows the
/// readers. Each of those has to be a local of its own in the function that
/// wants the searcher.
fn readers_of<'a>(segments: &[Segment<'a>]) -> Result<Vec<Reader<'a>>, Failure> {
    segments
        .iter()
        .map(|segment| Ok(Reader::open(segment)?))
        .collect()
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
    let (segments, region) = parts_of(&bytes, verify)?;
    let readers = readers_of(&segments)?;
    let searcher = Searcher::over(&readers)?;
    let index = &bytes[region];

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
            let ((hits, total), counters) = residency::measured(index, || {
                let (hits, counters) = searcher.search_explained(&text, k)?;
                Ok::<_, Failure>(((hits, None), counters))
            })?;
            (hits, total, counters)
        }
        (true, true) => {
            let ((hits, total), counters) = residency::measured(index, || {
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
        report::plan(&text, &readers, &mut std::io::stdout()).map_err(Failure::Stdout)?;
        report::counters(&counters, took, walk, &mut std::io::stdout()).map_err(Failure::Stdout)?;
    }

    match total {
        Some(total) => println!("{total} matching, showing {}", hits.len()),
        None => println!("showing {}", hits.len()),
    }
    let mut scratch = Scratch::new();
    for (rank, hit) in hits.iter().enumerate() {
        let named = label(&searcher, hit.doc, PATH_FIELD, &mut scratch)?
            .unwrap_or_else(|| format!("doc {}", hit.doc));
        println!("{:>3}  {:>8.4}  {named}", rank + 1, hit.score);
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
    /// A store could not be opened, added to or committed, and which one.
    Store(PathBuf, Trouble),
    /// An index was read through and found to be damaged, and how badly.
    Damaged(PathBuf, usize),
    /// A migration would not touch the file, and the report said why.
    Refused(PathBuf),
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
            Self::Store(path, trouble) => write!(f, "{}: {trouble}", path.display()),
            Self::Damaged(path, count) => {
                write!(f, "{}: {count} checks failed", path.display())
            }
            Self::Refused(path) => {
                write!(f, "{}: nothing was written, see above", path.display())
            }
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

    /// A directory of this test's own, named after the test that asked for it.
    fn scratch_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("kura-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("a directory to work in");
        path
    }

    const WORDS: [&str; 6] = ["ledger", "invoice", "quarter", "ledger", "audit", "ledger"];

    /// Writes `count` files of made up prose into `dir`, numbered from `from`.
    fn documents(dir: &Path, from: usize, count: usize) -> PathBuf {
        fs::create_dir_all(dir).expect("a directory");
        for n in from..from + count {
            let mut text = String::new();
            for step in 0..=(n % 5) {
                text.push_str(WORDS[(n + step) % WORDS.len()]);
                text.push(' ');
            }
            fs::write(dir.join(format!("{n}.txt")), text).expect("a document");
        }
        dir.to_path_buf()
    }

    /// The page a query gets out of an index file, as scores alone, which is
    /// what has to match across two files holding the same corpus. The labels
    /// cannot match: they are the paths the documents were indexed from, and the
    /// two indexes were built from different directories.
    fn scores(index: &Path, query: &str, k: usize) -> Vec<f32> {
        let bytes = Map::open(index).expect("a mapped index");
        let segments = segments_in(&bytes, true).expect("segments");
        let readers = readers_of(&segments).expect("readers");
        let searcher = Searcher::over(&readers).expect("a searcher");
        searcher
            .search(query, k)
            .expect("searched")
            .into_iter()
            .map(|hit| hit.score)
            .collect()
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the two pages are the same arithmetic over the same corpus, so \
                  anything but an exact match is the merge getting it wrong"
    )]
    fn a_store_of_four_segments_answers_the_way_one_segment_does() {
        // The end of the chain that started with the reader searching across
        // segments. Four separate runs of `index --store` over a quarter of the
        // corpus each, against one run over all of it, asked the same questions.
        let dir = scratch_dir("store-vs-segment");
        let corpus = documents(&dir.join("corpus"), 0, 120);

        let whole = dir.join("whole.kura");
        index(&[
            corpus.to_string_lossy().into_owned(),
            "-o".into(),
            whole.to_string_lossy().into_owned(),
        ])
        .expect("one segment");

        let store = dir.join("many.kura");
        for part in 0..4 {
            let piece = documents(&dir.join(format!("part{part}")), part * 30, 30);
            index(&[
                piece.to_string_lossy().into_owned(),
                "-o".into(),
                store.to_string_lossy().into_owned(),
                "--store".into(),
            ])
            .expect("a segment in a store");
        }

        for query in ["ledger", "invoice quarter", "ledger audit invoice"] {
            let one = scores(&whole, query, 20);
            let four = scores(&store, query, 20);
            assert_eq!(one.len(), four.len(), "{query}");
            for (at, (a, b)) in one.iter().zip(&four).enumerate() {
                assert_eq!(a, b, "{query} at rank {}", at + 1);
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the two pages are the same arithmetic over the same corpus, so \
                  anything but an exact match is the merge getting it wrong"
    )]
    fn flushing_part_way_through_answers_the_way_one_segment_does() {
        // The same claim as the test above, made about one run rather than four,
        // which is the case a corpus that does not arrive in pieces produces.
        // The same files, in the same order, so the two indexes differ in how
        // many segments they are cut into and in nothing else.
        let dir = scratch_dir("flush-vs-segment");
        let corpus = documents(&dir.join("corpus"), 0, 200);

        let whole = dir.join("whole.kura");
        index(&[
            corpus.to_string_lossy().into_owned(),
            "-o".into(),
            whole.to_string_lossy().into_owned(),
        ])
        .expect("one segment");

        let cut = dir.join("cut.kura");
        index(&[
            corpus.to_string_lossy().into_owned(),
            "-o".into(),
            cut.to_string_lossy().into_owned(),
            "--store".into(),
            "--flush-every".into(),
            "512".into(),
        ])
        .expect("several segments");

        let bytes = Map::open(&cut).expect("mapped");
        let held = segments_in(&bytes, true).expect("segments").len();
        assert!(held > 1, "the corpus went into {held} segments");

        for query in ["ledger", "invoice quarter", "ledger audit invoice"] {
            let one = scores(&whole, query, 20);
            let many = scores(&cut, query, 20);
            assert_eq!(one.len(), many.len(), "{query}");
            for (at, (a, b)) in one.iter().zip(&many).enumerate() {
                assert_eq!(a, b, "{query} at rank {}", at + 1);
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_document_survives_being_cut_into_segments() {
        // A flush that dropped the document it flushed on, or counted one twice,
        // would still answer queries and would answer them over the wrong
        // corpus, so the count is worth asserting separately from the scores.
        let dir = scratch_dir("flush-counts");
        let corpus = documents(&dir.join("corpus"), 0, 137);

        let cut = dir.join("cut.kura");
        index(&[
            corpus.to_string_lossy().into_owned(),
            "-o".into(),
            cut.to_string_lossy().into_owned(),
            "--store".into(),
            "--flush-every".into(),
            "1k".into(),
        ])
        .expect("several segments");

        let store = Store::open(&cut).expect("the store opens");
        assert_eq!(store.manifest().total, 137);
        assert_eq!(store.manifest().live, 137);
        let counted: u64 = store
            .manifest()
            .segments
            .iter()
            .map(|segment| u64::from(segment.docs))
            .sum();
        assert_eq!(counted, 137);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_budget_nothing_reaches_leaves_one_segment_and_no_empty_one() {
        // A corpus smaller than the budget never flushes, so the whole of it
        // comes out of the last flush, and there must not be a second segment
        // holding nothing behind it.
        let dir = scratch_dir("flush-never");
        let corpus = documents(&dir.join("corpus"), 0, 10);

        let cut = dir.join("cut.kura");
        index(&[
            corpus.to_string_lossy().into_owned(),
            "-o".into(),
            cut.to_string_lossy().into_owned(),
            "--store".into(),
            "--flush-every".into(),
            "1g".into(),
        ])
        .expect("one segment");

        let store = Store::open(&cut).expect("the store opens");
        assert_eq!(store.manifest().segments.len(), 1);
        assert_eq!(store.manifest().total, 10);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flushing_needs_somewhere_to_flush_into() {
        let dir = scratch_dir("flush-bare");
        let corpus = documents(&dir.join("corpus"), 0, 4);
        let refused = index(&[
            corpus.to_string_lossy().into_owned(),
            "-o".into(),
            dir.join("bare.kura").to_string_lossy().into_owned(),
            "--flush-every".into(),
            "1k".into(),
        ]);
        assert!(refused.is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_size_takes_a_unit_or_goes_without_one() {
        assert_eq!(size("4096").expect("plain bytes"), 4096);
        assert_eq!(size("64k").expect("kibibytes"), 64 << 10);
        assert_eq!(size("2M").expect("mebibytes"), 2 << 20);
        assert_eq!(size("1g").expect("gibibytes"), 1 << 30);
    }

    #[test]
    fn a_size_that_is_not_one_is_refused_rather_than_guessed_at() {
        // Nothing here should turn into a number by accident, and a zero is the
        // one that would otherwise be accepted and then flush after every
        // document.
        for value in ["", "k", "-1", "1kb", "1.5g", "0", "0k", "many"] {
            assert!(size(value).is_err(), "{value} was taken as a size");
        }
        assert!(size("99999999999g").is_err());
    }

    #[test]
    fn a_store_is_told_from_a_segment_by_its_first_bytes() {
        let dir = scratch_dir("told-apart");
        let corpus = documents(&dir.join("corpus"), 0, 8);

        let bare = dir.join("bare.kura");
        index(&[
            corpus.to_string_lossy().into_owned(),
            "-o".into(),
            bare.to_string_lossy().into_owned(),
        ])
        .expect("one segment");
        let store = dir.join("store.kura");
        index(&[
            corpus.to_string_lossy().into_owned(),
            "-o".into(),
            store.to_string_lossy().into_owned(),
            "--store".into(),
        ])
        .expect("a store");

        let bare = Map::open(&bare).expect("mapped");
        assert!(!manifest::looks_like_a_store(&bare));
        assert_eq!(segments_in(&bare, true).expect("segments").len(), 1);

        let store = Map::open(&store).expect("mapped");
        assert!(manifest::looks_like_a_store(&store));
        assert_eq!(segments_in(&store, true).expect("segments").len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_plan_says_how_many_segments_the_query_covers() {
        let dir = scratch_dir("plan-segments");
        let store = dir.join("store.kura");
        for part in 0..3 {
            let piece = documents(&dir.join(format!("part{part}")), part * 10, 10);
            index(&[
                piece.to_string_lossy().into_owned(),
                "-o".into(),
                store.to_string_lossy().into_owned(),
                "--store".into(),
            ])
            .expect("a segment in a store");
        }

        let bytes = Map::open(&store).expect("mapped");
        let segments = segments_in(&bytes, true).expect("segments");
        let readers = readers_of(&segments).expect("readers");
        let mut out = Vec::new();
        report::plan("ledger nonesuch", &readers, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("ascii");

        assert!(text.contains("segments 3 searched together"), "{text}");
        // Absent everywhere is absent. Present in one is present, with the count
        // summed over the three.
        assert!(text.contains("absent"), "{text}");
        assert!(text.contains("terms    1 of 2"), "{text}");

        let _ = fs::remove_dir_all(&dir);
    }
}
