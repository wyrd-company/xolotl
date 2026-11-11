//! Every link in the docs must reach something its reader can open.
//!
//! wyrd.foo publishes every `.md` page under docs/user whose front matter says
//! `product: xolotl`, the assets that docs.yml lists, and the two changelogs.
//! Nothing else in the repository is on the site, so a page links only to
//! another page, to a listed asset, to a heading, or to a full URL. A page
//! links to another page as `page.md` or `page.md#heading`, the form that also
//! works on GitHub.
//!
//! The changelogs are made from the intents in .intentional/intents, and are
//! shown away from the pages, so their links must be full URLs.
//!
//! The README is read on GitHub: its relative links must reach files in the
//! repository.
//!
//! Every full URL into the published docs on wyrd.foo must name a published
//! page and heading.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use pulldown_cmark::{BrokenLink, CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};
use yaml_rust2::{Yaml, YamlLoader};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const DOCS: &str = "docs/user";
const PRODUCT: &str = "xolotl";
const SITE_DOCS: &str = "https://wyrd.foo/docs/xolotl/";
const REPO: &str = "https://github.com/wyrd-company/xolotl/";
/// Files in the repository that a published page may link to on GitHub: a
/// reader of the site needs them and they are not on the site.
const REPO_FILES_FOR_READERS: &[&str] = &["config.example.toml", "LICENSE"];
/// The docs unit changelog, relative to docs/user. It is published, but it is
/// not a page.
const DOCS_CHANGELOG: &str = "CHANGELOG.md";

#[derive(Debug)]
struct Link {
    line: usize,
    target: String,
}

struct Document {
    links: Vec<Link>,
    /// Heading anchors, as wyrd.foo and GitHub make them.
    anchors: BTreeSet<String>,
    /// `[text][label]` and `[text][]` with no definition for the label.
    broken_references: Vec<Link>,
    /// The source, which the publisher searches for asset paths.
    text: String,
}

struct Site {
    /// Published pages, by path relative to docs/user.
    pages: BTreeMap<String, Document>,
    /// Assets that docs.yml lists, relative to docs/user.
    listed: BTreeSet<String>,
    /// The listed assets the publisher copies, relative to docs/user.
    assets: BTreeSet<String>,
    /// The docs.yml nav, in order.
    nav: Vec<String>,
}

impl Site {
    fn read() -> Site {
        let docs = Path::new(ROOT).join(DOCS);
        let mut pages = BTreeMap::new();
        for path in markdown_files(&docs) {
            let rel = path.strip_prefix(&docs).unwrap().to_str().unwrap().replace('\\', "/");
            if rel == DOCS_CHANGELOG {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            if page_product(&source).as_deref() == Some(PRODUCT) {
                pages.insert(rel, parse(&source));
            }
        }
        let (listed, named, nav) = docs_yml();
        let assets = published_assets(&listed, &named, &pages);
        Site { pages, listed, assets, nav }
    }

    /// The published page with this slug: its file name without `.md`.
    fn page_by_slug(&self, slug: &str) -> Option<(&String, &Document)> {
        self.pages.iter().find(|(rel, _)| slug_of(rel) == slug)
    }
}

#[test]
fn published_pages_link_only_to_published_content() {
    let site = Site::read();
    let mut problems = Vec::new();
    for (rel, page) in &site.pages {
        let file = format!("{DOCS}/{rel}");
        report_broken_references(&file, page, &mut problems);
        for link in &page.links {
            if let Some(problem) = page_link_problem(&site, rel, &link.target) {
                problems.push(format!("{file}:{}: {}: {problem}", link.line, link.target));
            }
        }
    }
    assert_no_problems(problems);
}

#[test]
fn every_page_is_published_and_in_the_nav() {
    let site = Site::read();
    let docs = Path::new(ROOT).join(DOCS);
    let mut problems = Vec::new();
    for path in markdown_files(&docs) {
        let rel = path.strip_prefix(&docs).unwrap().to_str().unwrap().replace('\\', "/");
        if rel != DOCS_CHANGELOG && !site.pages.contains_key(&rel) {
            problems.push(format!("{DOCS}/{rel}: front matter has no `product: {PRODUCT}`, so wyrd.foo does not publish it"));
        }
    }
    let mut slugs = BTreeSet::new();
    for rel in site.pages.keys() {
        let slug = slug_of(rel);
        if !slugs.insert(slug) {
            problems.push(format!("{DOCS}/{rel}: another page has the slug {slug}"));
        }
        if !site.nav.iter().any(|n| n == slug) {
            problems.push(format!("{DOCS}/{rel}: the docs.yml nav does not list {slug}"));
        }
    }
    for slug in &site.nav {
        if site.page_by_slug(slug).is_none() {
            problems.push(format!("{DOCS}/docs.yml: the nav lists {slug}, but no published page has that slug"));
        }
    }
    assert_no_problems(problems);
}

#[test]
fn changelog_and_intent_links_are_full_urls() {
    let site = Site::read();
    let mut files = vec![PathBuf::from("CHANGELOG.md"), Path::new(DOCS).join(DOCS_CHANGELOG)];
    let intents = Path::new(ROOT).join(".intentional/intents");
    if intents.is_dir() {
        files.extend(markdown_files(&intents).into_iter().map(|p| p.strip_prefix(ROOT).unwrap().to_path_buf()));
    }
    let mut problems = Vec::new();
    for file in files {
        let Ok(source) = std::fs::read_to_string(Path::new(ROOT).join(&file)) else { continue };
        let doc = parse(&source);
        let file = file.display().to_string();
        report_broken_references(&file, &doc, &mut problems);
        for link in &doc.links {
            if let Some(problem) = changelog_link_problem(&site, &link.target) {
                problems.push(format!("{file}:{}: {}: {problem}", link.line, link.target));
            }
        }
    }
    assert_no_problems(problems);
}

#[test]
fn readme_links_reach_files_and_published_pages() {
    let site = Site::read();
    let readme = parse(&std::fs::read_to_string(Path::new(ROOT).join("README.md")).unwrap());
    let mut problems = Vec::new();
    report_broken_references("README.md", &readme, &mut problems);
    for link in &readme.links {
        if let Some(problem) = repo_link_problem(&site, &readme, &link.target) {
            problems.push(format!("README.md:{}: {}: {problem}", link.line, link.target));
        }
    }
    assert_no_problems(problems);
}

// --- tests of the rules ---------------------------------------------------------

#[test]
fn anchors_follow_the_heading_text() {
    let doc = parse("# Point the CLI at the server\n## `xolotl keys` and `--json`\n## Rate limits\n## Rate limits\n");
    let anchors: Vec<_> = doc.anchors.into_iter().collect();
    assert_eq!(anchors, ["point-the-cli-at-the-server", "rate-limits", "rate-limits-1", "xolotl-keys-and---json"]);
}

#[test]
fn undefined_references_are_reported() {
    let doc = parse("[full][nowhere] and [collapsed][] and [shortcut prose] and [ok][def]\n\n[def]: https://example.com\n");
    let labels: Vec<_> = doc.broken_references.iter().map(|l| l.target.as_str()).collect();
    assert_eq!(labels, ["nowhere", "collapsed"]);
}

#[test]
fn html_links_are_read_from_attributes() {
    let doc = parse(concat!(
        "<img SRC=\"a.png\"> <a href = 'b.md'>b</a> <img src=c.png alt=x>\n\n",
        "<!-- <a href=\"hidden.md\"> -->\n\n",
        "<div>\n<!--\n<img src=\"hidden.png\">\n-->\n<a data-href=\"no.md\" href=\"d.md?x=1&amp;y=2\">d</a>\n</div>\n",
        "<img alt=\"a > b\" src=\"e.png\"> <a href=\"f&#46;md#&#x67;\">f</a>\n",
    ));
    let targets: Vec<_> = doc.links.iter().map(|l| l.target.as_str()).collect();
    assert_eq!(targets, ["a.png", "b.md", "c.png", "d.md?x=1&y=2", "e.png", "f.md#g"]);
}

#[test]
fn page_links_are_checked_against_the_published_set() {
    let mut pages = BTreeMap::new();
    pages.insert("one.md".to_string(), parse("# One\n## Run it\n"));
    pages.insert("two.md".to_string(), parse("# Two\n"));
    pages.insert("guides/three.md".to_string(), parse("# Three\n"));
    let listed = BTreeSet::from(["assets/demo.gif".to_string(), "assets/logo.svg".to_string(), "assets/gone.png".to_string()]);
    let site = Site { pages, listed, assets: BTreeSet::from(["assets/demo.gif".to_string()]), nav: vec![] };
    let ok = [
        "two.md",
        "two.md#two",
        "./two.md",
        "guides/three.md#three",
        "#run-it",
        "assets/demo.gif",
        "https://example.com/x",
        "https://wyrd.foo/docs/xolotl/three#three",
        "https://wyrd.foo/docs/xolotl/",
        "https://github.com/wyrd-company/xolotl/blob/main/config.example.toml",
        "https://github.com/wyrd-company/xolotl/releases",
    ];
    for target in ok {
        assert_eq!(page_link_problem(&site, "one.md", target), None, "{target}");
    }
    assert_eq!(page_link_problem(&site, "guides/three.md", "../one.md#run-it"), None);
    let refused = [
        "two",
        "two.md#missing",
        "#missing",
        "four.md",
        "three.md",
        "../design/README.md",
        "/docs/xolotl/two",
        "assets/other.png",
        "assets/gone.png",
        "assets/logo.svg",
        "CHANGELOG.md",
        "https://github.com/wyrd-company/xolotl/blob/main/docs/design/x.md",
        "https://wyrd.foo/docs/xolotl/four",
    ];
    for target in refused {
        assert!(page_link_problem(&site, "one.md", target).is_some(), "{target} must be refused");
    }
}

#[test]
fn the_publisher_copies_listed_assets_that_are_named_or_written_out() {
    let listed: BTreeSet<String> =
        ["assets/demo.gif", "assets/logo.svg", "assets/favicon.svg", "assets/social.png"].map(String::from).into();
    let named = BTreeSet::from(["assets/logo.svg".to_string()]);
    let pages = BTreeMap::from([
        ("one.md".to_string(), parse("![x](assets/demo.gif)\n")),
        ("guides/two.md".to_string(), parse("![x](../assets/favicon.svg) ![y](assets/./social.png)\n")),
    ]);
    let published: Vec<_> = published_assets(&listed, &named, &pages).into_iter().collect();
    assert_eq!(published, ["assets/demo.gif", "assets/favicon.svg", "assets/logo.svg"]);
    assert_eq!(relative("guides", "assets/x.png"), "../assets/x.png");
    assert_eq!(relative("", "assets/x.png"), "assets/x.png");
}

#[test]
fn readme_and_changelog_rules() {
    let site = Site {
        pages: BTreeMap::from([("two.md".to_string(), parse("# Two\n"))]),
        listed: BTreeSet::new(),
        assets: BTreeSet::new(),
        nav: vec![],
    };
    let readme = parse("# Top\n");
    for ok in [
        "src/metadata.rs",
        "src/metadata.rs#L10",
        "src/metadata.rs#L10-L20",
        "docs/user/web-ui.md",
        "#top",
        "https://github.com/wyrd-company/xolotl/blob/main/src/metadata.rs",
    ] {
        assert_eq!(repo_link_problem(&site, &readme, ok), None, "{ok}");
    }
    for refused in
        ["src/nowhere.rs", "src/metadata.rs#top", "docs/user/web-ui.md#nowhere", "/src/metadata.rs", "https://wyrd.foo/docs/xolotl/four"]
    {
        assert!(repo_link_problem(&site, &readme, refused).is_some(), "{refused} must be refused");
    }
    assert_eq!(changelog_link_problem(&site, "https://wyrd.foo/docs/xolotl/two"), None);
    for refused in ["two.md", "#two", "https://github.com/wyrd-company/xolotl/blob/main/src/metadata.rs"] {
        assert!(changelog_link_problem(&site, refused).is_some(), "{refused} must be refused");
    }
}

// --- rules ------------------------------------------------------------------------

/// Why a link on the published page `rel` does not reach published content.
fn page_link_problem(site: &Site, rel: &str, target: &str) -> Option<String> {
    if let Some(anchor) = target.strip_prefix('#') {
        return anchor_problem(&site.pages[rel], anchor, "this page");
    }
    if is_url(target) {
        return published_url_problem(site, target);
    }
    if target.starts_with('/') {
        return Some("use a relative link, such as `page.md`; a root path does not resolve on GitHub".into());
    }
    let (path, anchor) = split_anchor(target);
    let page_dir = Path::new(DOCS).join(rel).parent().unwrap().to_path_buf();
    let Some(resolved) = normalize(&page_dir.join(percent_decode(path))) else {
        return Some("leaves the repository".into());
    };
    let Ok(within) = resolved.strip_prefix(DOCS) else {
        return Some(format!("{} is not published on wyrd.foo; link to a page in {DOCS} or to a full URL", resolved.display()));
    };
    let within = within.to_str().unwrap().replace('\\', "/");
    if within.ends_with(".md") {
        return match site.pages.get(&within) {
            Some(doc) => anchor.and_then(|a| anchor_problem(doc, a, &within)),
            None => Some(format!("{DOCS}/{within} is not a published page")),
        };
    }
    if site.pages.contains_key(&format!("{within}.md")) {
        return Some(format!("write `{path}.md`, so the link works on GitHub too"));
    }
    let exists = Path::new(ROOT).join(DOCS).join(&within).is_file();
    if site.assets.contains(&within) {
        return anchor.map(|_| "an asset has no headings".into());
    }
    if site.listed.contains(&within) {
        return Some(if exists {
            format!("no page has the literal path to {within} (such as `{within}` from docs/user), so the publisher does not copy it")
        } else {
            format!("docs.yml lists {within}, but {DOCS}/{within} does not exist")
        });
    }
    if exists {
        return Some(format!("{DOCS}/{within} is not in the assets list of docs.yml, so it is not published"));
    }
    Some(format!("{DOCS}/{within} does not exist"))
}

/// Why a link in a changelog or an intent is not a full URL to something a
/// reader can open.
fn changelog_link_problem(site: &Site, target: &str) -> Option<String> {
    if !is_url(target) {
        return Some("a changelog is shown away from the pages; use a full URL, such as https://wyrd.foo/docs/xolotl/<page>".into());
    }
    published_url_problem(site, target)
}

/// Why a link in a file read on GitHub does not reach a file.
fn repo_link_problem(site: &Site, doc: &Document, target: &str) -> Option<String> {
    if let Some(anchor) = target.strip_prefix('#') {
        return anchor_problem(doc, anchor, "this file");
    }
    if is_url(target) {
        return site_url_problem(site, target);
    }
    if target.starts_with('/') {
        return Some("use a path relative to this file".into());
    }
    let (path, anchor) = split_anchor(target);
    let Some(resolved) = normalize(Path::new(&percent_decode(path))) else {
        return Some("leaves the repository".into());
    };
    let full = Path::new(ROOT).join(&resolved);
    if !full.exists() {
        return Some(format!("{} does not exist", resolved.display()));
    }
    match anchor {
        Some(anchor) if full.extension().is_some_and(|e| e == "md") => {
            let other = parse(&std::fs::read_to_string(&full).unwrap());
            anchor_problem(&other, anchor, &resolved.display().to_string())
        }
        Some(line) if is_line_anchor(line) => None,
        Some(_) => Some("only a Markdown file has headings to link to; another file takes a line, such as #L10 or #L10-L20".into()),
        None => None,
    }
}

/// A full URL in published content: as for any file, plus links into this
/// repository on GitHub must name a file a reader of the site needs.
fn published_url_problem(site: &Site, target: &str) -> Option<String> {
    if let Some(problem) = site_url_problem(site, target) {
        return Some(problem);
    }
    let rest = target.strip_prefix(REPO)?;
    let path = rest.strip_prefix("blob/").or_else(|| rest.strip_prefix("tree/"))?;
    let (_, path) = path.split_once('/').unwrap_or((path, ""));
    let path = path.split(['#', '?']).next().unwrap();
    if REPO_FILES_FOR_READERS.contains(&path) {
        return None;
    }
    Some(format!(
        "{path} is a repository file, not user documentation; link to a published page, or add the file to REPO_FILES_FOR_READERS"
    ))
}

/// A full URL into the published docs must name a published page and heading.
fn site_url_problem(site: &Site, target: &str) -> Option<String> {
    let rest = target.strip_prefix(SITE_DOCS)?;
    let (slug, anchor) = split_anchor(rest);
    let slug = slug.trim_end_matches('/');
    if slug.is_empty() || slug == "changelog" {
        return None;
    }
    match site.page_by_slug(slug) {
        Some((rel, doc)) => anchor.and_then(|a| anchor_problem(doc, a, rel)),
        None => Some(format!("{slug} is not a published page")),
    }
}

/// A GitHub line fragment: `L10` or `L10-L20`.
fn is_line_anchor(anchor: &str) -> bool {
    let number = |s: &str| s.strip_prefix('L').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    match anchor.split_once('-') {
        Some((from, to)) => number(from) && number(to),
        None => number(anchor),
    }
}

fn anchor_problem(doc: &Document, anchor: &str, place: &str) -> Option<String> {
    let anchor = percent_decode(anchor);
    if doc.anchors.contains(&anchor) { None } else { Some(format!("{place} has no heading with the anchor #{anchor}")) }
}

fn report_broken_references(file: &str, doc: &Document, problems: &mut Vec<String>) {
    for link in &doc.broken_references {
        problems.push(format!("{file}:{}: [{}]: the reference has no definition", link.line, link.target));
    }
}

fn assert_no_problems(problems: Vec<String>) {
    assert!(problems.is_empty(), "{} broken or unpublished links:\n{}", problems.len(), problems.join("\n"));
}

// --- the published set -------------------------------------------------------------

/// The listed assets the publisher copies: each one that exists and that the
/// catalog entry names, or whose path relative to a page is in that page's text.
fn published_assets(listed: &BTreeSet<String>, named: &BTreeSet<String>, pages: &BTreeMap<String, Document>) -> BTreeSet<String> {
    listed
        .iter()
        .filter(|asset| Path::new(ROOT).join(DOCS).join(asset).is_file())
        .filter(|asset| {
            named.contains(*asset)
                || pages.iter().any(|(rel, doc)| doc.text.contains(&relative(rel.rsplit_once('/').map_or("", |(dir, _)| dir), asset)))
        })
        .cloned()
        .collect()
}

/// `path` relative to the folder `dir`; both are relative to docs/user.
fn relative(dir: &str, path: &str) -> String {
    let dir: Vec<_> = dir.split('/').filter(|p| !p.is_empty()).collect();
    let path: Vec<_> = path.split('/').collect();
    let common = dir.iter().zip(&path).take_while(|(a, b)| a == b).count();
    let mut parts = vec![".."; dir.len() - common];
    parts.extend(&path[common..]);
    parts.join("/")
}

/// The assets list, the assets the catalog entry names, and the nav slugs of
/// docs.yml.
fn docs_yml() -> (BTreeSet<String>, BTreeSet<String>, Vec<String>) {
    let source = std::fs::read_to_string(Path::new(ROOT).join(DOCS).join("docs.yml")).unwrap();
    let docs = YamlLoader::load_from_str(&source).unwrap().remove(0);
    let strings = |yaml: &Yaml| -> Vec<String> {
        yaml.as_vec().map(|v| v.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    let assets: BTreeSet<String> = strings(&docs["assets"]).into_iter().collect();
    let product = &docs["products"][PRODUCT];
    let mut named: BTreeSet<String> =
        ["logo", "logo-dark", "favicon", "social-image"].iter().filter_map(|f| product[*f].as_str().map(str::to_string)).collect();
    for item in product["media"].as_vec().into_iter().flatten() {
        named.extend(item["src"].as_str().map(str::to_string));
    }
    let mut nav = Vec::new();
    for section in docs["products"][PRODUCT]["nav"].as_vec().into_iter().flatten() {
        for group in section["groups"].as_vec().into_iter().flatten() {
            nav.extend(strings(&group["pages"]));
        }
    }
    assert!(!nav.is_empty(), "docs.yml has no nav pages for {PRODUCT}");
    (assets, named, nav)
}

fn page_product(source: &str) -> Option<String> {
    let front = source.strip_prefix("---\n")?.split_once("\n---")?.0;
    let yaml = YamlLoader::load_from_str(front).ok()?.into_iter().next()?;
    yaml["product"].as_str().map(str::to_string)
}

fn slug_of(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap().trim_end_matches(".md")
}

/// Every `.md` file under `dir`, at any depth, sorted.
fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

// --- Markdown -------------------------------------------------------------------------

fn parse(source: &str) -> Document {
    let line_of = |offset: usize| source[..offset].matches('\n').count() + 1;
    let broken = std::cell::RefCell::new(Vec::new());
    let mut callback = |link: BrokenLink<'_>| {
        if matches!(link.link_type, LinkType::Reference | LinkType::Collapsed) {
            broken.borrow_mut().push(Link { line: line_of(link.span.start), target: link.reference.to_string() });
        }
        None::<(CowStr<'_>, CowStr<'_>)>
    };
    let options = Options::ENABLE_TABLES | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_with_broken_link_callback(source, options, Some(&mut callback));

    let mut links = Vec::new();
    let mut anchors = BTreeSet::new();
    let mut heading: Option<String> = None;
    let mut html = HtmlScanner::default();
    for (event, range) in parser.into_offset_iter() {
        let line = line_of(range.start);
        match event {
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                links.push(Link { line, target: dest_url.to_string() });
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                links.extend(html.targets(&text).into_iter().map(|target| Link { line, target }));
            }
            Event::Start(Tag::Heading { .. }) => heading = Some(String::new()),
            Event::Text(text) | Event::Code(text) => {
                if let Some(h) = heading.as_mut() {
                    h.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                let base = slug(&heading.take().unwrap_or_default());
                let mut anchor = base.clone();
                let mut n = 0;
                while anchors.contains(&anchor) {
                    n += 1;
                    anchor = format!("{base}-{n}");
                }
                anchors.insert(anchor);
            }
            _ => {}
        }
    }
    Document { links, anchors, broken_references: broken.into_inner(), text: source.to_string() }
}

/// Reads `href` and `src` attribute values from raw HTML, which pulldown-cmark
/// hands over in pieces. A comment can span pieces.
#[derive(Default)]
struct HtmlScanner {
    in_comment: bool,
}

impl HtmlScanner {
    fn targets(&mut self, html: &str) -> Vec<String> {
        let mut targets = Vec::new();
        let mut rest = html;
        loop {
            if self.in_comment {
                let Some(end) = rest.find("-->") else { return targets };
                rest = &rest[end + 3..];
                self.in_comment = false;
            }
            let Some(open) = rest.find('<') else { return targets };
            rest = &rest[open..];
            if let Some(after) = rest.strip_prefix("<!--") {
                rest = after;
                self.in_comment = true;
                continue;
            }
            let end = tag_end(rest);
            targets.extend(tag_targets(&rest[1..end]));
            rest = &rest[end..];
            if rest.is_empty() {
                return targets;
            }
            rest = &rest[1..];
        }
    }
}

/// The `href` and `src` values among the attributes of one tag.
fn tag_targets(tag: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = tag.as_bytes();
    // Skip the tag name.
    let mut i = bytes.iter().position(|b| b.is_ascii_whitespace()).unwrap_or(bytes.len());
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'=' | b'/') {
            i += 1;
        }
        let name = tag[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = match bytes.get(i) {
            Some(&q @ (b'"' | b'\'')) => {
                let end = tag[i + 1..].find(q as char).map_or(bytes.len(), |e| i + 1 + e);
                let value = &tag[(i + 1).min(end)..end];
                i = end + 1;
                value
            }
            _ => {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                &tag[start..i]
            }
        };
        if name == "href" || name == "src" {
            targets.push(decode_entities(value));
        }
    }
    targets
}

/// The offset of the `>` that closes the tag at the start of `html`, outside
/// quoted attribute values.
fn tag_end(html: &str) -> usize {
    let mut quote = None;
    for (i, c) in html.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return i,
            _ => {}
        }
    }
    html.len()
}

/// Named entities that can appear in a URL, and numeric references.
fn decode_entities(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest.find(';').and_then(|end| {
            let name = &rest[1..end];
            let c = match name {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .map(|hex| u32::from_str_radix(hex, 16))
                    .or_else(|| name.strip_prefix('#').map(str::parse::<u32>))
                    .and_then(Result::ok)
                    .and_then(char::from_u32),
            };
            c.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A heading's anchor, as github-slugger makes it (wyrd.foo and GitHub use
/// it) for headings in ASCII: lowercase, punctuation removed, spaces as `-`.
fn slug(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' '))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

// --- paths ------------------------------------------------------------------------------

fn is_url(target: &str) -> bool {
    target
        .split_once(':')
        .is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
}

fn split_anchor(target: &str) -> (&str, Option<&str>) {
    let (path, anchor) = match target.split_once('#') {
        Some((path, anchor)) => (path, Some(anchor)),
        None => (target, None),
    };
    (path.split('?').next().unwrap(), anchor)
}

/// `path` with `.` and `..` removed; `None` when it climbs above the root.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
