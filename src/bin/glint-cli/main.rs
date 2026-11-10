//! glint-cli: command-line search over the same index the GUI uses.

use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use glint::engine::{Engine, Mode};
use glint::index::Index;

mod export;
use export::Format;
use glint::query::Query;
use glint::search::{self, SortKey};

const HELP: &str = "\
glint-cli: instant file search (NTFS MFT + USN journal)

USAGE: glint-cli [OPTIONS] [QUERY...]

OPTIONS:
  -n, --max <N>        print at most N results
  -s, --sort <KEY>     name (default) | path (by folder) | size | date
  -r, --reverse        reverse the sort order
  -c, --count          print only the number of matches
  -l, --long           include size and modification date
  -f, --format <F>     plain (default) | csv | tsv | json  (export with
                       path, name, size, modified, attributes, type)
      --mft <FILE>     search an offline $MFT copy instead of this PC's index
      --as <LETTER>    drive letter to show for --mft paths (default X)
      --reindex        rebuild the index from scratch
      --db <PATH>      index snapshot (default %LOCALAPPDATA%\\Glint\\index.db)
  -h, --help           this help

Run elevated once to index NTFS volumes from the MFT; later runs replay the
USN journal from the snapshot. QUERY syntax: see `Help > Search syntax` in
the GUI (foo bar, !not, a|b, *.rs, dir\\name, ext:, size:, file:, folder:, regex:,
dm:, attrib:, depth:, len:, parent:).";

struct Args {
    query: String,
    max: usize,
    sort: SortKey,
    reverse: bool,
    count: bool,
    long: bool,
    format: Format,
    mft: Option<PathBuf>,
    letter: u8,
    reindex: bool,
    db: PathBuf,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        query: String::new(),
        max: usize::MAX,
        sort: SortKey::Name,
        reverse: false,
        count: false,
        long: false,
        format: Format::Plain,
        mft: None,
        letter: b'X',
        reindex: false,
        db: glint::default_db_path(),
    };
    let mut words = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-n" | "--max" => {
                a.max = value("--max")?
                    .parse()
                    .map_err(|_| "--max needs a number")?
            }
            "-s" | "--sort" => {
                a.sort = match value("--sort")?.to_ascii_lowercase().as_str() {
                    "name" => SortKey::Name,
                    "path" => SortKey::Path,
                    "size" => SortKey::Size,
                    "date" | "dm" => SortKey::Date,
                    other => return Err(format!("unknown sort key: {other}")),
                }
            }
            "-r" | "--reverse" => a.reverse = true,
            "-c" | "--count" => a.count = true,
            "-l" | "--long" => a.long = true,
            "-f" | "--format" => {
                a.format = match value("--format")?.to_ascii_lowercase().as_str() {
                    "plain" => Format::Plain,
                    "csv" => Format::Csv,
                    "tsv" => Format::Tsv,
                    "json" => Format::Json,
                    other => return Err(format!("unknown format: {other}")),
                }
            }
            "--mft" => a.mft = Some(PathBuf::from(value("--mft")?)),
            "--as" => {
                let v = value("--as")?;
                match v.trim_end_matches([':', '\\']).as_bytes() {
                    [c] if c.is_ascii_alphabetic() => a.letter = c.to_ascii_uppercase(),
                    _ => return Err(format!("--as needs a drive letter, got {v}")),
                }
            }
            "--reindex" => a.reindex = true,
            "--db" => a.db = PathBuf::from(value("--db")?),
            "--" => words.extend(it.by_ref()),
            _ => words.push(arg),
        }
    }
    a.query = words.join(" ");
    Ok(Some(a))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("glint-cli: {e}\nTry --help.");
            return ExitCode::from(2);
        }
    };
    let q = match Query::parse(&args.query) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("glint-cli: {e}");
            return ExitCode::from(2);
        }
    };

    let engine = Engine::new(args.db.clone(), |_| {});
    let image;
    let guard;
    let ix: &Index = match &args.mft {
        Some(path) => match glint::mft::scan_image(path, args.letter) {
            Ok(mut built) => {
                built.finalize();
                image = built;
                &image
            }
            Err(e) => {
                eprintln!("glint-cli: {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => {
            engine.startup(Mode::OneShot, args.reindex);
            guard = engine.read();
            &guard
        }
    };

    let found = search::run(ix, &q);
    let found = search::sort(ix, found, args.sort, args.reverse);

    if args.count {
        println!("{}", found.len());
    } else {
        let out = io::stdout().lock();
        let mut w = BufWriter::with_capacity(1 << 16, out);
        let res = export::write(
            &mut w,
            ix,
            found.iter().take(args.max),
            args.format,
            args.long,
        )
        .and_then(|()| w.flush());
        match res {
            // A closed pipe (`| head`) is a normal way to stop reading.
            Err(e) if e.kind() != io::ErrorKind::BrokenPipe => {
                eprintln!("glint-cli: writing results failed: {e}");
                return ExitCode::FAILURE;
            }
            _ => {}
        }
    }
    ExitCode::SUCCESS
}
