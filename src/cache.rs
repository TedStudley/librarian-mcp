use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::search::SearchIndex;
use crate::server::LibraryServer;

/// Node id for a vault-relative path: forward slashes, no `.md` extension
/// (the same form Obsidian writes inside `[[...]]`).
pub fn id_from_rel(rel: &str) -> String {
    let r = rel.replace('\\', "/");
    match r.strip_suffix(".md") {
        Some(s) => s.to_string(),
        None => r,
    }
}

/// Link target without `#heading` / `^block` suffix or `.md` extension.
fn clean_target(raw: &str) -> &str {
    let t = raw.split(|c| c == '#' || c == '^').next().unwrap_or("").trim();
    match t.len().checked_sub(3).and_then(|i| t.get(i..)) {
        Some(ext) if ext.eq_ignore_ascii_case(".md") => &t[..t.len() - 3],
        _ => t,
    }
}

/// Collapse `.` and `..` segments. None if the path escapes the vault root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// Resolves link references to node ids the way Obsidian does:
///   1. exact path from the vault root;
///   2. path relative to the linking file;
///   3. name / path-suffix match anywhere in the vault, preferring the
///      shortest full path, then byte-wise lexicographic order.
#[derive(Default, Clone)]
pub struct Resolver {
    ids: HashSet<String>,
    by_name: HashMap<String, Vec<String>>,
}

impl Resolver {
    pub fn new<I: IntoIterator<Item = String>>(ids: I) -> Self {
        let mut r = Resolver::default();
        for id in ids {
            if !r.ids.insert(id.clone()) {
                continue;
            }
            let name = id.rsplit('/').next().unwrap_or("").to_string();
            r.by_name.entry(name).or_default().push(id);
        }
        r
    }

    pub fn resolve(&self, source: &str, target: &str) -> Option<String> {
        let t = clean_target(target);
        if t.is_empty() {
            return None;
        }
        if self.ids.contains(t) {
            return Some(t.to_string());
        }
        if let Some(rooted) = t.strip_prefix('/') {
            return self.ids.contains(rooted).then(|| rooted.to_string());
        }

        let dir = source.rsplit_once('/').map_or("", |(d, _)| d);
        let joined = if dir.is_empty() { t.to_string() } else { format!("{}/{}", dir, t) };
        if let Some(p) = normalize(&joined) {
            if self.ids.contains(&p) {
                return Some(p);
            }
        }
        if t.split('/').any(|s| s == "." || s == "..") {
            return None;
        }

        let name = t.rsplit('/').next()?;
        let suffix = format!("/{}", t);
        self.by_name
            .get(name)?
            .iter()
            .filter(|id| id.as_str() == t || id.ends_with(&suffix))
            .min_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
            .cloned()
    }

    /// Every note a bare name or path-suffix could refer to, best match first,
    /// when more than one does (so callers can say which one they picked).
    pub fn ambiguous_matches(&self, input: &str) -> Vec<String> {
        let t = clean_target(input);
        if t.is_empty() || t.starts_with('/') || self.ids.contains(t) {
            return Vec::new();
        }
        let name = t.rsplit('/').next().unwrap_or("");
        let suffix = format!("/{}", t);
        let mut found: Vec<String> = self
            .by_name
            .get(name)
            .map(|ids| ids.iter().filter(|id| id.ends_with(&suffix)).cloned().collect())
            .unwrap_or_default();
        found.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        if found.len() > 1 { found } else { Vec::new() }
    }

    /// Note names shared by more than one file, with their node ids (sorted).
    pub fn duplicate_names(&self) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = self
            .by_name
            .iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(name, ids)| {
                let mut ids = ids.clone();
                ids.sort();
                (name.clone(), ids)
            })
            .collect();
        out.sort();
        out
    }

    /// True if a relative target climbs above the vault root (e.g. `../README`
    /// from a top-level note): such links point outside the vault, not at a
    /// missing note.
    pub fn escapes_vault(&self, source: &str, target: &str) -> bool {
        let t = clean_target(target);
        if t.is_empty() || t.starts_with('/') {
            return false;
        }
        let dir = source.rsplit_once('/').map_or("", |(d, _)| d);
        let joined = if dir.is_empty() { t.to_string() } else { format!("{}/{}", dir, t) };
        normalize(&joined).is_none()
    }

    /// Link text Obsidian writes in "shortest path" mode: the bare name when
    /// it is unique in the vault, otherwise the full vault-root path (even
    /// when `id` would win the name lookup, and even for a note in the
    /// linking file's own folder).
    pub fn link_text(&self, id: &str) -> String {
        let name = id.rsplit('/').next().unwrap_or(id);
        match self.by_name.get(name) {
            Some(ids) if ids.len() > 1 => id.to_string(),
            _ => name.to_string(),
        }
    }
}

impl Default for VaultCache {
    fn default() -> Self {
        VaultCache {
            search_index: SearchIndex::default(),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            titles: Vec::new(),
            file_mtimes: HashMap::new(),
            raw_refs: HashMap::new(),
            resolver: Resolver::default(),
        }
    }
}

pub struct VaultCache {
    pub search_index: SearchIndex,
    /// Graph keyed by node id (see `id_from_rel`). Dangling references are
    /// stored as `?<target>` so they stay distinguishable from real notes.
    pub outgoing: HashMap<String, Vec<String>>,
    pub incoming: HashMap<String, Vec<String>>,
    pub titles: Vec<(String, String, String)>, // (match_term, canonical stem, rel_path)
    pub resolver: Resolver,
    /// Unresolved link targets per file id; edges are re-derived from these
    /// whenever the file set changes, because resolution is vault-global.
    raw_refs: HashMap<String, Vec<String>>,
    file_mtimes: HashMap<PathBuf, SystemTime>,
}

impl VaultCache {
    /// Resolve a user-supplied path or name (e.g. "plans/x.md" or "x") to a
    /// node id, using the same rules as links.
    pub fn resolve_input(&self, input: &str) -> Option<String> {
        self.resolver.resolve("", input)
    }

    /// The other notes `input` could have meant, if it is ambiguous.
    pub fn input_ambiguity(&self, input: &str) -> Vec<String> {
        self.resolver.ambiguous_matches(input)
    }

    /// Re-derive the resolver and the edge maps after deferred updates.
    pub fn refresh_graph(&mut self) {
        self.rebuild_graph();
    }

    /// Re-derive the resolver and the edge maps from the raw references.
    fn rebuild_graph(&mut self) {
        self.resolver = Resolver::new(self.raw_refs.keys().cloned());
        self.outgoing.clear();
        self.incoming.clear();

        let mut sources: Vec<&String> = self.raw_refs.keys().collect();
        sources.sort();
        for id in sources {
            self.outgoing.entry(id.clone()).or_default();
            for raw in &self.raw_refs[id] {
                let cleaned = clean_target(raw);
                if cleaned.is_empty() || self.resolver.escapes_vault(id, raw) {
                    continue;
                }
                let target = self
                    .resolver
                    .resolve(id, raw)
                    .unwrap_or_else(|| format!("?{}", cleaned));
                self.outgoing.get_mut(id).unwrap().push(target.clone());
                self.incoming.entry(target).or_default().push(id.clone());
            }
        }
    }

    /// Build the full cache from scratch by reading all md files once.
    pub fn build_full(server: &LibraryServer) -> VaultCache {
        let mut search_files: Vec<(PathBuf, String)> = Vec::new();
        let mut raw_refs: HashMap<String, Vec<String>> = HashMap::new();
        let mut titles: Vec<(String, String, String)> = Vec::new();
        let mut file_mtimes: HashMap<PathBuf, SystemTime> = HashMap::new();

        for path in server.all_md_files() {
            let stem = match path.file_stem() {
                Some(s) => s.to_string_lossy().to_string(),
                None => continue,
            };
            let rel = server.relative_path(&path);

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Store mtime
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(mtime) = meta.modified() {
                    file_mtimes.insert(path.clone(), mtime);
                }
            }

            // Search index data
            search_files.push((path.clone(), content.clone()));

            raw_refs.insert(id_from_rel(&rel), LibraryServer::extract_links(&content));

            // Titles: stem + aliases
            titles.push((stem.clone(), stem.clone(), rel.clone()));
            for alias in LibraryServer::extract_aliases(&content) {
                titles.push((alias, stem.clone(), rel.clone()));
            }
        }

        let search_index = SearchIndex::build(&search_files);
        eprintln!("Librarian: indexed {} files for search", search_files.len());

        let mut cache = VaultCache {
            search_index,
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            titles,
            file_mtimes,
            raw_refs,
            resolver: Resolver::default(),
        };
        cache.rebuild_graph();
        cache
    }

    /// Check file mtimes and refresh changed/deleted/new files.
    pub fn check_and_refresh(&mut self, server: &LibraryServer) {
        let current_files = server.all_md_files();
        let current_set: std::collections::HashSet<PathBuf> =
            current_files.iter().cloned().collect();
        let mut changed = false;

        // Find deleted files (in cache but not on disk)
        let cached_paths: Vec<PathBuf> = self.file_mtimes.keys().cloned().collect();
        for path in cached_paths {
            if !current_set.contains(&path) {
                self.remove_file_entries(&path, server);
                self.file_mtimes.remove(&path);
                changed = true;
            }
        }

        // Find new or changed files
        for path in &current_files {
            let current_mtime = std::fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok());

            let needs_update = match (self.file_mtimes.get(path), current_mtime) {
                (Some(cached), Some(current)) => *cached != current,
                (None, _) => true, // new file
                _ => false,
            };

            if needs_update {
                if let Ok(content) = std::fs::read_to_string(path) {
                    self.remove_file_entries(path, server);
                    self.add_file_entries(path, &content, server);
                    if let Some(mtime) = current_mtime {
                        self.file_mtimes.insert(path.clone(), mtime);
                    }
                    changed = true;
                }
            }
        }

        if changed {
            self.rebuild_graph();
        }
    }

    /// Update a single file's cache entries (called after library_write).
    pub fn update_single_file(&mut self, path: &Path, content: &str, server: &LibraryServer) {
        self.update_file_deferred(path, content, server);
        self.rebuild_graph();
    }

    /// Like `update_single_file` but leaves the graph stale. Callers writing
    /// many files call `refresh_graph` once afterwards.
    pub fn update_file_deferred(&mut self, path: &Path, content: &str, server: &LibraryServer) {
        self.remove_file_entries(path, server);
        self.add_file_entries(path, content, server);

        // Update mtime
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(mtime) = meta.modified() {
                self.file_mtimes.insert(path.to_path_buf(), mtime);
            }
        }
    }

    /// Remove all non-graph cache entries for a file; the graph itself is
    /// re-derived by `rebuild_graph` once the batch of changes is done.
    fn remove_file_entries(&mut self, path: &Path, server: &LibraryServer) {
        let rel = server.relative_path(path);

        self.raw_refs.remove(&id_from_rel(&rel));
        self.titles.retain(|(_, _, r)| r != &rel);
        self.search_index.remove_file(path);
    }

    /// Add cache entries for a file from its content.
    fn add_file_entries(&mut self, path: &Path, content: &str, server: &LibraryServer) {
        let stem = match path.file_stem() {
            Some(s) => s.to_string_lossy().to_string(),
            None => return,
        };
        let rel = server.relative_path(path);

        self.raw_refs
            .insert(id_from_rel(&rel), LibraryServer::extract_links(content));

        // Add to titles
        self.titles.push((stem.clone(), stem.clone(), rel.clone()));
        for alias in LibraryServer::extract_aliases(content) {
            self.titles.push((alias, stem.clone(), rel.clone()));
        }

        // Add to search index
        self.search_index.add_file(path, content);
    }
}
