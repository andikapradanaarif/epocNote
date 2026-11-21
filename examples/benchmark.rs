//! Reproducible local corpus measurements for plan.md section 13.
use epocnote::db::Index;
use epocnote::search::{Limit, SearchInput, search};
use epocnote::sync::{IndexSync, ScanReuse};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn fixture(root: &Path, count: usize) -> std::io::Result<()> {
    let notes = root.join(".context/notes");
    fs::create_dir_all(&notes)?;
    let paragraph = "The session store keeps a revision counter. Check cache invalidation, database migration, recovery, and background worker retries before changing retention.\n\n";
    for number in 0..count {
        let shard = number % 100;
        let mut note = format!(
            "---\ntitle: Session case {number}\nkind: note\nimpacts: [src/sessions/shard{shard}]\n---\n\n# Session note\n\nRecord{number:06} covers shard {shard}.\n\n"
        );
        while note.len() < 5 * 1024 {
            note.push_str(paragraph);
        }
        note.truncate(5 * 1024 - 1);
        note.push('\n');
        fs::write(notes.join(format!("case-{number:06}.md")), note)?;
    }
    Ok(())
}

fn query(index: &Index, sync: &IndexSync, text: Option<&str>) -> epocnote::search::SearchOutput {
    search(
        index,
        sync,
        SearchInput {
            query: text.map(str::to_owned),
            path_filter: Some("src/sessions/shard42/state.rs".into()),
            limit: Limit::Value(5),
        },
        ScanReuse::Auto,
    )
    .unwrap()
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn measure(root: &Path, count: usize) {
    assert!(
        !epocnote::root::db_path(root).exists(),
        "use a fresh fixture for each measurement"
    );
    let start = Instant::now();
    let index = Index::open(root).unwrap();
    let sync = IndexSync::new(root);
    let first = query(&index, &sync, Some("record000042"));
    let time_to_first = start.elapsed();
    while sync.timings().worker_running {
        assert!(
            start.elapsed() < Duration::from_secs(900),
            "indexing timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let total = start.elapsed();
    let times = sync.timings();
    let indexed: i64 = index
        .conn()
        .query_row("SELECT count(*) FROM files", [], |row| row.get(0))
        .unwrap();
    assert_eq!(indexed, count as i64);
    println!("notes={count} sqlite={}", rusqlite::version());
    println!("scan_ms={:.2}", millis(times.scan_duration));
    println!(
        "first_answer_ms={:.2} partial_index={}",
        millis(time_to_first),
        first.partial_index
    );
    println!("total_index_ms={:.2}", millis(total));
    println!("max_write_lock_ms={:.2}", millis(times.max_write_lock));

    // Refresh the scan outside the sample set if it expired during indexing.
    let start = Instant::now();
    let warmed = query(&index, &sync, Some("record000042"));
    assert!(!warmed.partial_index && !warmed.results.is_empty());
    println!("post_index_search_ms={:.2}", millis(start.elapsed()));
    for (label, text) in [("text", Some("record000042")), ("path", None)] {
        let mut samples = Vec::new();
        for _ in 0..15 {
            let start = Instant::now();
            let result = query(&index, &sync, text);
            samples.push(start.elapsed());
            assert!(!result.partial_index && !result.results.is_empty());
        }
        samples.sort_unstable();
        println!(
            "warm_{label}_ms median={:.2} p95={:.2}",
            millis(samples[7]),
            millis(samples[14])
        );
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 4 {
        eprintln!("usage: benchmark fixture|measure <root> <note-count>");
        std::process::exit(2);
    }
    let root = Path::new(&args[2]);
    let count: usize = args[3].to_str().unwrap().parse().unwrap();
    assert!(count > 42, "fixture needs at least 43 notes");
    match args[1].to_str() {
        Some("fixture") => fixture(root, count).unwrap(),
        Some("measure") => measure(root, count),
        _ => panic!("expected fixture or measure"),
    }
}
