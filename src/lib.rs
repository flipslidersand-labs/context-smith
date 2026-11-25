mod budget;
mod bundle_writer;
mod dep_builder;
#[cfg_attr(not(feature = "remote-embed"), allow(dead_code))]
mod embed;
mod expand;
mod extract;
mod git;
mod index_builder;
mod language;
mod path_kind;
mod search_index;
mod select;
mod tokenizer;

pub use budget::Candidate;
pub use bundle_writer::{render_task_md, write_bundle};
#[cfg(feature = "remote-embed")]
pub use embed::embed_index;
pub(crate) use extract::SymbolExtractor;
pub use git::GitRepo;
#[cfg(test)]
pub(crate) use git::{is_path_escape, MAX_FILE_BYTES};
pub use index_builder::{build_index, IndexDb, IndexStats};
pub(crate) use language::{Language, SymbolKind};
pub use select::{select_candidates, SeedMode, SelectError, SelectOptions, Selection};

#[cfg(test)]
#[path = "tests/mod.rs"]
mod moved;

#[derive(Debug, Clone)]
pub(crate) struct Symbol {
    pub file_id: i64,
    pub name: String,
    pub kind: SymbolKind,
}

/// Shared helpers for unit tests that need a real git repository.
#[cfg(test)]
pub(crate) mod test_util {
    use std::path::Path;

    /// Write `add` files, delete `remove` files, and commit the result on HEAD.
    /// Initialises the repository on first use.
    pub fn commit_files(dir: &Path, add: &[(&str, &str)], remove: &[&str]) {
        let repo = git2::Repository::open(dir)
            .or_else(|_| git2::Repository::init(dir))
            .unwrap();
        let mut index = repo.index().unwrap();
        for (rel, content) in add {
            let abs = dir.join(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&abs, content).unwrap();
            index.add_path(Path::new(rel)).unwrap();
        }
        for rel in remove {
            std::fs::remove_file(dir.join(rel)).unwrap();
            index.remove_path(Path::new(rel)).unwrap();
        }
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, "test commit", &tree, &parents)
            .unwrap();
    }
}
