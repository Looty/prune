//! Debug scan, read-only:
//!   cargo run -p prune-core --example scan -- [provider ids...] [--top N] [-v] [--plan id:pkg,...]
use prune_core::{clean, report, scan};

fn main() {
    let mut ids = Vec::new();
    let mut top = 15;
    let mut verbose = false;
    let mut plan: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--top" => top = args.next().and_then(|v| v.parse().ok()).unwrap_or(15),
            "-v" | "--verbose" => verbose = true,
            "--plan" => plan = args.next(),
            _ => ids.push(a),
        }
    }
    if std::env::var_os("NO_VERIFY").is_none() {
        scan::set_verify(true);
    }
    let t = std::time::Instant::now();
    let reports = scan::scan_all(if ids.is_empty() { None } else { Some(&ids) });
    print!("{}", report::render(&reports, &report::Opts { top, verbose }));
    println!("total scan wall time: {} ms", t.elapsed().as_millis());

    if let Some(spec) = plan {
        let mut items = Vec::new();
        for part in spec.split(',') {
            let (prov, pkg) = part.split_once(':').unwrap_or((part, "*"));
            for r in reports.iter().filter(|r| r.id == prov) {
                for p in r.packages.iter().filter(|p| pkg == "*" || p.name == pkg) {
                    items.push((r.id, p));
                }
            }
        }
        print!("{}", clean::describe(&clean::plan(&items)));
    }
}
