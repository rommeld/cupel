use std::path::PathBuf;

use cupel_coding_agent::search::{
    CodeSearch, GrepSearch, SearchLimit, SearchQuery, source_locator,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .ok_or("supply the benchmark checkout path")?;
    let root = PathBuf::from(root).canonicalize()?;
    let backend = GrepSearch::new(&root);
    let mut cases = include_str!("grep-ranking.tsv").lines();
    let revision = cases.next().ok_or("benchmark revision is missing")?;
    let mut correct = 0;
    let mut total = 0;
    for case in cases {
        let (symbol, expected) = case.split_once('\t').ok_or("invalid benchmark row")?;
        let query = SearchQuery {
            pattern: symbol.to_string(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: true,
            context: 0,
            limit: SearchLimit::Files {
                max_files: 1,
                per_file: 20,
            },
        };
        let outcome = backend.search(query, CancellationToken::new()).await?;
        let first = outcome
            .ranked_files
            .first()
            .map(|file| source_locator(&file.path, &root));
        if first.as_deref() == Some(expected) {
            correct += 1;
        } else {
            let actual = first.as_deref().unwrap_or("no match");
            println!("{symbol}: expected {expected}, got {actual}");
        }
        total += 1;
    }
    println!("Hit@1: {correct}/{total} (checkout {revision})");
    if total != 25 || correct < 24 {
        return Err("expected at least 24 first-place hits out of 25 queries".into());
    }
    Ok(())
}
