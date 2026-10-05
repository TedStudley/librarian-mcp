use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rmcp::handler::server::router::tool::ToolRouter;

use crate::cache::{id_from_rel, Resolver, VaultCache};

/// Obsidian's "New link format" (Settings → Files & links).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkFormat {
    Shortest,
    Relative,
    Absolute,
}

/// How links the server writes are spelled, mirroring the vault's own
/// Obsidian settings (`useMarkdownLinks`, `newLinkFormat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkStyle {
    Wikilink,
    Markdown(LinkFormat),
}

/// Librarian MCP Server — give Claude a librarian for your markdown vault
#[derive(Clone)]
pub struct LibraryServer {
    /// One or more vault roots
    pub library_paths: Vec<PathBuf>,
    /// Default exclusion patterns when no .librarianignore exists
    pub default_ignores: Vec<String>,
    /// Lowercased note stems/aliases the auto-linker must never link
    /// (generic structural filenames like INDEX/README/SKILL that match
    /// common words and pollute the graph with cross-domain false edges).
    pub link_stoplist: Vec<String>,
    /// Top-level folders that must stay self-contained: no auto-created link
    /// may cross their boundary in either direction (e.g. a fiction book dir).
    /// Loaded from `.librarianisolate` in the vault root.
    pub isolated_folders: Vec<String>,
    /// When false, `library_write` / `library_import` never insert links.
    /// (`library_suggest_links` still reports suggestions.)
    pub auto_link: bool,
    /// Spelling of links the server writes.
    pub link_style: LinkStyle,
    /// Unified vault cache (search index, graph, titles)
    pub cache: std::sync::Arc<Mutex<VaultCache>>,
    pub tool_router: ToolRouter<Self>,
}

/// Generic stems excluded from auto-linking by default. These are structural
/// or template filenames whose stems collide with everyday prose words, so
/// matching them creates noise rather than meaningful links. Extend per-vault
/// with a `.librarianstoplist` file (one term per line) in the vault root.
pub const DEFAULT_LINK_STOPLIST: &[&str] = &[
    "claude", "skill", "index", "readme", "memory", "language",
    "changelog", "filename", "critic-prompt", "scoring-rubric",
];

impl LibraryServer {
    /// Resolve a relative path against vault roots. Returns the first match,
    /// or falls back to the first vault root for new files.
    /// Build the auto-link stoplist: the hardcoded defaults plus any terms
    /// listed in a `.librarianstoplist` file at any vault root. All lowercased.
    pub fn build_link_stoplist(library_paths: &[PathBuf]) -> Vec<String> {
        let mut stop: HashSet<String> =
            DEFAULT_LINK_STOPLIST.iter().map(|s| s.to_string()).collect();
        for root in library_paths {
            if let Ok(contents) = std::fs::read_to_string(root.join(".librarianstoplist")) {
                for line in contents.lines() {
                    let term = line.trim();
                    if !term.is_empty() && !term.starts_with('#') {
                        stop.insert(term.to_lowercase());
                    }
                }
            }
        }
        stop.into_iter().collect()
    }

    /// Load isolated top-level folder names from `.librarianisolate` files
    /// (one folder per line; trailing slashes and `#` comments ignored).
    pub fn build_isolated_folders(library_paths: &[PathBuf]) -> Vec<String> {
        let mut set: HashSet<String> = HashSet::new();
        for root in library_paths {
            if let Ok(contents) = std::fs::read_to_string(root.join(".librarianisolate")) {
                for line in contents.lines() {
                    let f = line.trim().trim_end_matches('/').trim();
                    if !f.is_empty() && !f.starts_with('#') {
                        set.insert(f.to_string());
                    }
                }
            }
        }
        set.into_iter().collect()
    }

    /// Link style from the first vault's `.obsidian/app.json`
    /// (`useMarkdownLinks`, `newLinkFormat`); `LIBRARIAN_LINK_STYLE=wikilink|markdown`
    /// overrides the wikilink/markdown choice. Defaults match Obsidian's own.
    pub fn detect_link_style(library_paths: &[PathBuf]) -> LinkStyle {
        let mut use_markdown = false;
        let mut format = LinkFormat::Shortest;
        if let Some(root) = library_paths.first() {
            if let Ok(text) = std::fs::read_to_string(root.join(".obsidian").join("app.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    use_markdown = v.get("useMarkdownLinks").and_then(|x| x.as_bool()).unwrap_or(false);
                    format = match v.get("newLinkFormat").and_then(|x| x.as_str()) {
                        Some("relative") => LinkFormat::Relative,
                        Some("absolute") => LinkFormat::Absolute,
                        _ => LinkFormat::Shortest,
                    };
                }
            }
        }
        match std::env::var("LIBRARIAN_LINK_STYLE").ok().as_deref() {
            Some("wikilink") => LinkStyle::Wikilink,
            Some("markdown") => LinkStyle::Markdown(format),
            _ if use_markdown => LinkStyle::Markdown(format),
            _ => LinkStyle::Wikilink,
        }
    }

    /// Percent-encode the characters that would break a markdown link target.
    fn encode_link_path(path: &str) -> String {
        let mut out = String::with_capacity(path.len());
        for ch in path.chars() {
            if " %#?:()[]<>\"".contains(ch) {
                out.push_str(&format!("%{:02X}", ch as u32));
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// Path from `source_id`'s folder to `target_id`, spelled so that the
    /// resolver maps it back to the target. Same-folder targets always get an
    /// explicit `./`: a bare `Foo.md` is only safe while no root-level
    /// `Foo.md` exists, and the point is to never become ambiguous.
    fn relative_link_path(resolver: &Resolver, source_id: &str, target_id: &str) -> String {
        let src: Vec<&str> = source_id.split('/').collect();
        let src_dirs = &src[..src.len() - 1];
        let tgt: Vec<&str> = target_id.split('/').collect();
        let tgt_dirs = &tgt[..tgt.len() - 1];
        let common = src_dirs.iter().zip(tgt_dirs).take_while(|(a, b)| a == b).count();
        let ups = src_dirs.len() - common;
        let mut parts: Vec<&str> = vec![".."; ups];
        parts.extend(&tgt[common..]);
        let rel = format!("{}.md", parts.join("/"));
        let vault = format!("{}.md", target_id);

        let mut candidates = vec![if ups == 0 { format!("./{}", rel) } else { rel }];
        candidates.push(vault.clone());
        for c in candidates {
            if resolver.resolve(source_id, &c).as_deref() == Some(target_id) {
                return Self::encode_link_path(&c);
            }
        }
        Self::encode_link_path(&vault)
    }

    /// Spell a link from `source_id` to `target_id` in the vault's style.
    /// `display` is the visible text; None uses the note's name.
    pub fn format_link(
        &self,
        resolver: &Resolver,
        source_id: &str,
        target_id: &str,
        display: Option<&str>,
    ) -> String {
        let name = target_id.rsplit('/').next().unwrap_or(target_id);
        match self.link_style {
            LinkStyle::Wikilink => {
                let text = resolver.link_text(target_id);
                match display {
                    Some(d) if d != name => format!("[[{}|{}]]", text, d),
                    _ => format!("[[{}]]", text),
                }
            }
            LinkStyle::Markdown(format) => {
                let path = match format {
                    LinkFormat::Relative => Self::relative_link_path(resolver, source_id, target_id),
                    LinkFormat::Shortest | LinkFormat::Absolute => {
                        Self::encode_link_path(&format!("{}.md", target_id))
                    }
                };
                format!("[{}]({})", display.unwrap_or(name), path)
            }
        }
    }

    /// Top-level directory component of a vault-relative path ("" if none).
    pub fn top_folder(rel: &str) -> &str {
        rel.split('/').next().unwrap_or("")
    }

    /// True if a link between two folders would cross an isolation boundary:
    /// the folders differ and at least one of them is isolated.
    pub fn crosses_isolation(&self, dir_a: &str, dir_b: &str) -> bool {
        dir_a != dir_b
            && (self.isolated_folders.iter().any(|f| f == dir_a)
                || self.isolated_folders.iter().any(|f| f == dir_b))
    }

    pub fn resolve_path(&self, rel: &str) -> PathBuf {
        for root in &self.library_paths {
            let candidate = root.join(rel);
            if candidate.exists() {
                return candidate;
            }
        }
        self.library_paths[0].join(rel)
    }

    /// Collect all markdown files across all vault roots, respecting .librarianignore.
    pub fn all_md_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for root in &self.library_paths {
            let mut builder = ignore::WalkBuilder::new(root);
            builder.hidden(true);

            let ignore_file = root.join(".librarianignore");
            if ignore_file.exists() {
                builder.add_ignore(ignore_file);
            } else {
                builder.filter_entry(move |entry| {
                    let path_str = entry.path().to_string_lossy();
                    !path_str.contains(".obsidian")
                        && !path_str.contains(".trash")
                        && !path_str.contains("node_modules")
                });
            }

            for entry in builder.build().flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "md") && path.is_file() {
                    files.push(path.to_path_buf());
                }
            }
        }
        files
    }

    /// Get the relative path of an absolute path, trying each vault root.
    pub fn relative_path(&self, abs: &Path) -> String {
        for root in &self.library_paths {
            if let Ok(rel) = abs.strip_prefix(root) {
                return rel.to_string_lossy().to_string();
            }
        }
        abs.to_string_lossy().to_string()
    }

    pub fn extract_frontmatter(content: &str) -> Option<String> {
        if content.starts_with("---\n") {
            if let Some(end) = content[4..].find("\n---") {
                return Some(content[4..4 + end].to_string());
            }
        }
        None
    }

    /// Extract aliases from YAML frontmatter.
    pub fn extract_aliases(content: &str) -> Vec<String> {
        let fm = match Self::extract_frontmatter(content) {
            Some(fm) => fm,
            None => return Vec::new(),
        };
        for line in fm.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("aliases:") {
                let rest = rest.trim();
                if rest.starts_with('[') {
                    return rest.trim_start_matches('[').trim_end_matches(']')
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                if !rest.is_empty() {
                    return vec![rest.trim_matches('"').trim_matches('\'').to_string()];
                }
            }
        }
        Vec::new()
    }

    /// Return byte ranges within `text` that should be excluded from auto-linking.
    /// Covers fenced code blocks, inline code, URLs, and existing wikilinks.
    fn find_exclusion_zones(text: &str) -> Vec<(usize, usize)> {
        let mut zones = Vec::new();

        // Fenced code blocks: ```...```
        let fenced = regex::Regex::new(r"(?ms)^```[^\n]*\n.*?^```").unwrap();
        for m in fenced.find_iter(text) {
            zones.push((m.start(), m.end()));
        }

        // Inline code: `...`
        let inline = regex::Regex::new(r"`[^`]+`").unwrap();
        for m in inline.find_iter(text) {
            zones.push((m.start(), m.end()));
        }

        // URLs: http:// or https:// until whitespace
        let urls = regex::Regex::new(r"https?://\S+").unwrap();
        for m in urls.find_iter(text) {
            zones.push((m.start(), m.end()));
        }

        // Existing wikilinks: [[...]]
        let wikilinks = regex::Regex::new(r"\[\[[^\]]+\]\]").unwrap();
        for m in wikilinks.find_iter(text) {
            zones.push((m.start(), m.end()));
        }

        // Existing markdown links and images: [text](target)
        let md_links = regex::Regex::new(r"!?\[[^\]]*\]\([^)]*\)").unwrap();
        for m in md_links.find_iter(text) {
            zones.push((m.start(), m.end()));
        }

        zones
    }

    /// `auto_link_content`, unless auto-linking is switched off.
    pub fn maybe_auto_link(&self, content: &str, exclude_path: &str, titles: &[(String, String, String)]) -> (String, Vec<String>) {
        if !self.auto_link {
            return (content.to_string(), Vec::new());
        }
        self.auto_link_content(content, exclude_path, titles)
    }

    /// Auto-link: scan content for mentions of existing note titles and link them
    /// in the vault's link style (wikilinks by default).
    pub fn auto_link_content(&self, content: &str, exclude_path: &str, titles: &[(String, String, String)]) -> (String, Vec<String>) {
        let existing_links = Self::extract_wikilinks(content);
        let existing_set: HashSet<&str> = existing_links.iter().map(|s| s.as_str()).collect();

        // Notes this content already links to, by any link style.
        let resolver = Resolver::new(titles.iter().map(|(_, _, rel)| id_from_rel(rel)));
        let source_id = id_from_rel(exclude_path);
        let linked_ids: HashSet<String> = Self::extract_links(content)
            .iter()
            .filter_map(|l| resolver.resolve(&source_id, l))
            .collect();

        let mut result = content.to_string();
        let mut links_added = Vec::new();

        let writing_dir = Self::top_folder(exclude_path);
        let mut candidates: Vec<_> = titles.iter()
            .filter(|(match_term, canonical, rel)| {
                match_term.len() >= 3
                    && rel != exclude_path
                    && !linked_ids.contains(&id_from_rel(rel))
                    && !self.link_stoplist.contains(&match_term.to_lowercase())
                    && !self.crosses_isolation(writing_dir, Self::top_folder(rel))
                    && !existing_set.contains(canonical.as_str())
                    && !existing_set.contains(match_term.as_str())
            })
            .collect();
        // A name that matches several notes can't be attributed to any one of
        // them from prose alone: link it only if exactly one is in the writing
        // note's own folder.
        let dir_of = |rel: &str| rel.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_string());
        let src_dir = dir_of(exclude_path);
        let mut rels_by_term: std::collections::HashMap<String, Vec<&str>> = std::collections::HashMap::new();
        for (match_term, _, rel) in &candidates {
            let rels = rels_by_term.entry(match_term.to_lowercase()).or_default();
            if !rels.contains(&rel.as_str()) {
                rels.push(rel.as_str());
            }
        }
        let keep: Vec<bool> = candidates
            .iter()
            .map(|(match_term, _, rel)| {
                let rels = &rels_by_term[&match_term.to_lowercase()];
                rels.len() <= 1
                    || (dir_of(rel) == src_dir
                        && rels.iter().filter(|r| dir_of(r) == src_dir).count() == 1)
            })
            .collect();
        let mut keep_iter = keep.into_iter();
        candidates.retain(|_| keep_iter.next().unwrap_or(false));
        candidates.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        let mut linked_stems: HashSet<String> = HashSet::new();

        for (match_term, canonical, rel) in &candidates {
            if linked_stems.contains(canonical.as_str()) {
                continue;
            }

            let pattern = format!(r"(?i)\b{}\b", regex::escape(match_term));
            if let Ok(re) = regex::Regex::new(&pattern) {
                let fm_end = if result.starts_with("---\n") {
                    result[4..].find("\n---\n").map(|i| 4 + i + 5).unwrap_or(0)
                } else {
                    0
                };

                let body_part = &result[fm_end..];

                if body_part.contains(&format!("[[{}]]", canonical))
                    || body_part.contains(&format!("[[{}|", canonical))
                {
                    continue;
                }

                let exclusion_zones = Self::find_exclusion_zones(body_part);

                // Find the first match that doesn't overlap an exclusion zone
                let mut search_start = 0;
                let found = loop {
                    if search_start >= body_part.len() {
                        break None;
                    }
                    match re.find(&body_part[search_start..]) {
                        Some(m) => {
                            let abs_start = search_start + m.start();
                            let abs_end = search_start + m.end();
                            let overlaps = exclusion_zones.iter().any(|(zs, ze)| {
                                abs_start < *ze && abs_end > *zs
                            });
                            if overlaps {
                                // Advance past this match and keep searching
                                search_start = abs_end;
                            } else {
                                break Some((abs_start, abs_end));
                            }
                        }
                        None => break None,
                    }
                };

                if let Some((m_start, m_end)) = found {
                    let matched_text = &body_part[m_start..m_end];
                    let target_id = id_from_rel(rel);
                    let display = if match_term.to_lowercase() == canonical.to_lowercase() {
                        None
                    } else {
                        Some(matched_text)
                    };
                    let replacement = match self.link_style {
                        // Wikilinks keep their legacy shape: the canonical name.
                        LinkStyle::Wikilink => self.format_link(&resolver, &source_id, &target_id, display),
                        // Markdown links show the text as written in the note.
                        LinkStyle::Markdown(_) => {
                            self.format_link(&resolver, &source_id, &target_id, Some(matched_text))
                        }
                    };
                    let new_body = format!(
                        "{}{}{}",
                        &body_part[..m_start],
                        replacement,
                        &body_part[m_end..]
                    );
                    result = format!("{}{}", &result[..fm_end], new_body);
                    links_added.push(canonical.to_string());
                    linked_stems.insert(canonical.to_string());
                }
            }
        }

        (result, links_added)
    }

    pub fn extract_wikilinks(content: &str) -> Vec<String> {
        let re = regex::Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]").unwrap();
        re.captures_iter(content)
            .map(|c| c[1].to_string())
            .collect()
    }

    /// Copy of `content` with everything Obsidian does not index links from
    /// blanked out (same byte length, newlines kept): fenced code, HTML
    /// comments, indented code blocks and inline code.
    pub fn mask_non_prose(content: &str) -> String {
        static FENCE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static COMMENT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static INLINE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static LIST: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let fence = FENCE.get_or_init(|| {
            regex::Regex::new(r"(?ms)^ {0,3}(?:```|~~~)[^\n]*\n.*?^ {0,3}(?:```|~~~)").unwrap()
        });
        let comment = COMMENT.get_or_init(|| regex::Regex::new(r"(?s)<!--.*?-->").unwrap());
        let inline = INLINE.get_or_init(|| regex::Regex::new(r"`[^`\n]+`").unwrap());
        let list = LIST.get_or_init(|| regex::Regex::new(r"^\s*(?:[-*+]|\d{1,9}[.)])\s").unwrap());

        fn blank(bytes: &mut [u8], start: usize, end: usize) {
            for b in &mut bytes[start..end] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
        // The masked buffer only ever has whole characters replaced by spaces.
        fn text(bytes: &[u8]) -> String {
            String::from_utf8_lossy(bytes).into_owned()
        }

        let mut bytes = content.as_bytes().to_vec();
        for m in fence.find_iter(content) {
            blank(&mut bytes, m.start(), m.end());
        }
        let after_fences = text(&bytes);
        for m in comment.find_iter(&after_fences) {
            blank(&mut bytes, m.start(), m.end());
        }

        // Indented code: a 4-space/tab run that follows a blank line, outside
        // any list (inside a list it is a continuation paragraph).
        let current = text(&bytes);
        let mut offset = 0usize;
        let (mut prev_blank, mut in_list, mut in_code) = (true, false, false);
        for line in current.split_inclusive('\n') {
            let (start, end) = (offset, offset + line.len());
            offset = end;
            let body = line.trim_end_matches('\n');
            if body.trim().is_empty() {
                prev_blank = true;
                continue;
            }
            let indented = body.starts_with("    ") || body.starts_with('\t');
            if in_code && indented {
                blank(&mut bytes, start, end);
                continue;
            }
            in_code = false;
            if indented && prev_blank && !in_list {
                in_code = true;
                blank(&mut bytes, start, end);
                prev_blank = false;
                continue;
            }
            if list.is_match(body) {
                in_list = true;
            } else if !indented {
                in_list = false;
            }
            prev_blank = false;
        }

        let after_indent = text(&bytes);
        for m in inline.find_iter(&after_indent) {
            blank(&mut bytes, m.start(), m.end());
        }
        text(&bytes)
    }

    /// Targets of relative markdown links (`[text](path.md#anchor)`), percent-
    /// decoded and without the anchor. Skips links inside code or comments,
    /// URLs with a scheme, bare `#anchor` links and anything that is not a
    /// `.md` target.
    pub fn extract_markdown_links(content: &str) -> Vec<String> {
        Self::markdown_links_in(&Self::mask_non_prose(content))
    }

    fn markdown_links_in(masked: &str) -> Vec<String> {
        static LINK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static SCHEME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let link = LINK.get_or_init(|| {
            regex::Regex::new(r#"\[[^\]]*\]\(\s*(<[^>]+>|[^)\s]+)(?:\s+(?:"[^"]*"|'[^']*'))?\s*\)"#).unwrap()
        });
        let scheme = SCHEME.get_or_init(|| regex::Regex::new(r"^[A-Za-z][A-Za-z0-9+.\-]*:").unwrap());

        let mut out = Vec::new();
        for caps in link.captures_iter(masked) {
            let href = caps[1].trim_start_matches('<').trim_end_matches('>');
            if scheme.is_match(href) {
                continue;
            }
            let path = href.split('#').next().unwrap_or("");
            let decoded = Self::percent_decode(path);
            if decoded.to_lowercase().ends_with(".md") {
                out.push(decoded);
            }
        }
        out
    }

    /// All link targets in a note: wikilinks (verbatim) then markdown links,
    /// ignoring anything inside code or HTML comments.
    pub fn extract_links(content: &str) -> Vec<String> {
        let masked = Self::mask_non_prose(content);
        let mut links = Self::extract_wikilinks(&masked);
        links.extend(Self::markdown_links_in(&masked));
        links
    }

    fn percent_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// Graph node id for an absolute path inside a vault.
    pub fn node_id(&self, abs: &Path) -> String {
        crate::cache::id_from_rel(&self.relative_path(abs))
    }

    pub fn extract_tags(content: &str) -> Vec<String> {
        let re = regex::Regex::new(r"(?:^|\s)#([\w/-]+)").unwrap();
        re.captures_iter(content)
            .map(|c| c[1].to_string())
            .collect()
    }

}
