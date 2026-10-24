//! A markdown document store, and the tool that opens one document.
//!
//! Documents are plain files with YAML-ish front matter:
//!
//! ```markdown
//! ---
//! name: tdd
//! description: Test-driven development. Use when the user wants to build
//!   features or fix bugs test-first.
//! ---
//!
//! The body, in markdown.
//! ```
//!
//! Only each document's `name` and `description` reach the model up front, as
//! an index in the preamble. Bodies are opened on demand through the
//! `read_skill` tool, so a large library costs a few hundred tokens a turn
//! rather than its full weight.
//!
//! Nothing here is required to run the application: with no directory
//! configured, a missing one, or an unreadable file, startup continues and the
//! problem is reported as a note.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use rig::tool::{Tool, ToolContext};
use serde::Deserialize;

/// How deep to descend while looking for documents.
///
/// Skill collections nest a category and a name (`engineering/tdd/SKILL.md`);
/// a bound keeps a symlink cycle from walking forever.
const MAX_DEPTH: usize = 6;

/// The file name that marks a directory as one skill.
const SKILL_FILE: &str = "SKILL.md";

/// One document: what the model sees up front, and where the body lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    /// The name the model opens the document by.
    pub id: String,
    /// One line telling the model when this document is worth opening.
    pub description: String,
    /// Where the body is read from, when it is asked for.
    pub path: PathBuf,
    /// Whether the model may open this itself.
    ///
    /// Front matter's `disable-model-invocation` marks a document the user
    /// triggers deliberately — an interview, a handoff — which a model should
    /// not start on its own. Those are reachable as `/name` instead.
    pub model_invocable: bool,
}

/// Every document found, indexed by id.
#[derive(Debug, Default, Clone)]
pub struct Docs {
    docs: BTreeMap<String, Doc>,
    /// Human-readable notes about what was (or was not) loaded.
    pub notes: Vec<String>,
}

impl Docs {
    /// Load every `SKILL.md` under `root`.
    ///
    /// `None`, a missing directory, or an unreadable file yields no documents
    /// and no error: the library is opt-in, exactly like MCP.
    pub fn load(root: Option<&Path>) -> Self {
        let mut loaded = Self::default();

        let Some(root) = root else {
            return loaded;
        };
        if !root.is_dir() {
            loaded
                .notes
                .push(format!("skills: no directory at {}", root.display()));
            return loaded;
        }

        let mut paths = Vec::new();
        collect(root, 0, &mut paths, &mut loaded.notes);
        paths.sort();

        for path in paths {
            match read_doc(&path) {
                Ok(doc) => {
                    if let Some(previous) = loaded.docs.insert(doc.id.clone(), doc) {
                        // Two files claiming one name would make `read_skill`
                        // ambiguous, so say which one lost.
                        loaded.notes.push(format!(
                            "skills: duplicate name `{}`, ignoring {}",
                            previous.id,
                            previous.path.display()
                        ));
                    }
                }
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "skipping document");
                    loaded
                        .notes
                        .push(format!("skills: {} ({err})", path.display()));
                }
            }
        }

        if !loaded.docs.is_empty() {
            loaded
                .notes
                .push(format!("skills: {} loaded", loaded.docs.len()));
        }
        loaded
    }

    /// Whether any document was found.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// How many documents were found.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Every document's name, for the interface's slash commands.
    pub fn ids(&self) -> Vec<String> {
        self.docs.keys().cloned().collect()
    }

    /// The catalogue shown to the model, or `None` when there is nothing to
    /// show.
    ///
    /// One line per document. This is the only part that costs tokens every
    /// turn, which is why it carries the description and not the body.
    pub fn index(&self) -> Option<String> {
        // A library of nothing but user-invoked documents has no catalogue to
        // show the model, even though it is not empty.
        if !self.docs.values().any(|doc| doc.model_invocable) {
            return None;
        }
        let mut index = String::from(
            "\n\nYou have a library of skills: reference documents with \
             instructions for particular kinds of work. Open one with the \
             read_skill tool when its description matches what is being asked, \
             and follow it. A skill may name another; open that one too. Do \
             not guess at a skill's contents from its description alone.\
             \n\nAvailable skills:\n",
        );
        for doc in self.docs.values().filter(|doc| doc.model_invocable) {
            index.push_str(&format!("- {}: {}\n", doc.id, doc.description));
        }
        Some(index)
    }

    /// The body of the document named `id`, read from disk now.
    ///
    /// Reading late rather than at startup means an edited skill takes effect
    /// on the next call instead of the next run.
    pub fn body(&self, id: &str) -> Result<String, SkillError> {
        let doc = self.docs.get(id).ok_or_else(|| SkillError::Unknown {
            id: id.to_string(),
            known: self.docs.keys().cloned().collect::<Vec<_>>().join(", "),
        })?;
        std::fs::read_to_string(&doc.path).map_err(|source| SkillError::Unreadable {
            id: id.to_string(),
            source,
        })
    }
}

/// What can go wrong when opening a skill.
#[derive(Debug)]
pub enum SkillError {
    /// No skill goes by that name.
    Unknown { id: String, known: String },
    /// The file is listed but could not be read.
    Unreadable { id: String, source: std::io::Error },
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The known names go to the model so a near miss can be corrected
            // without another round trip.
            Self::Unknown { id, known } => {
                write!(f, "no skill named `{id}`. Available skills: {known}")
            }
            Self::Unreadable { id, source } => write!(f, "could not read skill `{id}`: {source}"),
        }
    }
}

impl std::error::Error for SkillError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unknown { .. } => None,
            Self::Unreadable { source, .. } => Some(source),
        }
    }
}

/// Gather every `SKILL.md` under `dir`, depth-first.
fn collect(dir: &Path, depth: usize, found: &mut Vec<PathBuf>, notes: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            notes.push(format!("skills: cannot read {} ({err})", dir.display()));
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // A dotfile directory here is `.git` or an editor's, never a skill.
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        if path.is_dir() {
            collect(&path, depth + 1, found, notes);
        } else if path.file_name().is_some_and(|name| name == SKILL_FILE) {
            found.push(path);
        }
    }
}

/// Read one document's front matter.
fn read_doc(path: &Path) -> Result<Doc, String> {
    let contents = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let fields = parse_front_matter(&contents).ok_or("no front matter")?;

    // `name` is authoritative, but a collection may omit it and rely on the
    // directory, which is how the file is referred to anyway.
    let id = fields
        .get("name")
        .cloned()
        .or_else(|| {
            path.parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .ok_or("no name")?;
    let description = fields.get("description").cloned().ok_or("no description")?;

    if id.trim().is_empty() || description.trim().is_empty() {
        return Err("empty name or description".into());
    }

    // Absent means invocable: a document says so only to opt out.
    let model_invocable = fields
        .get("disable-model-invocation")
        .map(|value| {
            // Matched loosely and case-insensitively: this gate fails in the
            // unsafe direction, so an unrecognised `True` would leave a
            // document the user meant to trigger deliberately open to the
            // model.
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1"
            )
        })
        .unwrap_or(true);

    Ok(Doc {
        id: id.trim().to_string(),
        description: description.trim().to_string(),
        path: path.to_path_buf(),
        model_invocable,
    })
}

/// Parse the `---` fenced header into key/value pairs.
///
/// This understands the subset of YAML that document front matter actually
/// uses: plain scalars, quoted scalars, and folded or literal block scalars.
/// Anything else is skipped rather than guessed at, which is why a document
/// without a usable `description` is reported instead of loaded.
///
/// Known edges of the subset, none of which real front matter has hit yet:
/// a trailing `# comment` stays part of the value, because a description may
/// legitimately contain `#`; an unbalanced quote is kept rather than
/// rejected; a nested mapping is flattened into its parent's value; and `|`
/// folds like `>` rather than preserving newlines. Reach for a YAML crate if
/// a document ever needs one of these.
fn parse_front_matter(contents: &str) -> Option<BTreeMap<String, String>> {
    // The closing fence is a line that is exactly `---`, not any line that
    // starts with it: a `----` rule, or a `---` inside a block scalar, must
    // not end the header early and spill the rest of it into the body.
    let mut all = contents.lines();
    if all.next()?.trim_end() != "---" {
        return None;
    }
    let mut header = Vec::new();
    let mut closed = false;
    for line in all {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        header.push(line);
    }
    if !closed {
        return None;
    }

    let mut fields = BTreeMap::new();
    let mut lines = header.into_iter().peekable();

    while let Some(line) = lines.next() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        // Only a top-level key starts a field; an indented line belongs to the
        // value above it and is consumed below.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let rest = rest.trim();

        let value = if rest.starts_with('>') || rest.starts_with('|') {
            // A block scalar: every following indented line is part of it.
            let mut parts = Vec::new();
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() {
                    lines.next();
                    continue;
                }
                if !next.starts_with(char::is_whitespace) {
                    break;
                }
                parts.push(lines.next().unwrap_or_default().trim().to_string());
            }
            parts.join(" ")
        } else {
            let mut value = unquote(rest).to_string();
            // A plain scalar may also continue on indented lines.
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() || !next.starts_with(char::is_whitespace) {
                    break;
                }
                let continuation = lines.next().unwrap_or_default();
                value.push(' ');
                value.push_str(continuation.trim());
            }
            value
        };

        fields.insert(key, value);
    }

    Some(fields)
}

/// Strip one layer of matching quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Arguments to [`ReadSkill`].
#[derive(Debug, Deserialize)]
pub struct ReadSkillArgs {
    /// The skill's name, exactly as the index spells it.
    pub name: String,
}

/// The tool that opens one skill.
///
/// Holds the catalogue rather than a path so an unknown name can be answered
/// with the list of known ones, and so the index and the tool can never
/// disagree about what exists.
#[derive(Debug, Clone)]
pub struct ReadSkill {
    docs: Docs,
}

impl ReadSkill {
    pub fn new(docs: Docs) -> Self {
        Self { docs }
    }
}

impl Tool for ReadSkill {
    const NAME: &'static str = "read_skill";

    type Args = ReadSkillArgs;
    type Output = String;
    type Error = SkillError;

    fn description(&self) -> String {
        "Open a skill from the library and read its instructions. Takes the \
         skill's name as listed in the preamble."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill's name, exactly as the index spells it.",
                },
            },
            "required": ["name"],
        })
    }

    fn annotations(&self) -> rig::completion::ToolAnnotations {
        rig::completion::ToolAnnotations {
            title: Some("Read skill".to_string()),
            read_only_hint: Some(true),
            open_world_hint: Some(false),
            ..Default::default()
        }
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let body = self.docs.body(args.name.trim())?;
        tracing::debug!(skill = %args.name, "skill opened");
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory holding `files` as `name/SKILL.md`.
    fn library(label: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rigagent-skills-{label}"));
        let _ = std::fs::remove_dir_all(&root);
        for (name, contents) in files {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).expect("create skill dir");
            std::fs::write(dir.join(SKILL_FILE), contents).expect("write skill");
        }
        root
    }

    #[test]
    fn plain_front_matter_is_parsed() {
        let fields = parse_front_matter("---\nname: tdd\ndescription: Test first.\n---\nBody\n")
            .expect("front matter");
        assert_eq!(fields.get("name").map(String::as_str), Some("tdd"));
        assert_eq!(
            fields.get("description").map(String::as_str),
            Some("Test first.")
        );
    }

    #[test]
    fn wrapped_and_quoted_values_are_joined() {
        let fields = parse_front_matter(
            "---\nname: \"grilling\"\ndescription: Grill the user\n  about a plan,\n  relentlessly.\n---\n",
        )
        .expect("front matter");
        assert_eq!(fields.get("name").map(String::as_str), Some("grilling"));
        assert_eq!(
            fields.get("description").map(String::as_str),
            Some("Grill the user about a plan, relentlessly."),
            "a wrapped plain scalar joins into one line"
        );
    }

    #[test]
    fn block_scalars_are_folded() {
        let fields = parse_front_matter(
            "---\nname: research\ndescription: >-\n  Investigate a\n  question.\n---\n",
        )
        .expect("front matter");
        assert_eq!(
            fields.get("description").map(String::as_str),
            Some("Investigate a question.")
        );
    }

    #[test]
    fn only_an_exact_fence_closes_the_header() {
        // A `---` inside a block scalar, and a `----` rule, used to end the
        // header early and spill the rest of it into the body.
        let fields = parse_front_matter(
            "---\nname: rules\ndescription: |\n  before\n  ---\n  after\n----\n---\nBody\n",
        )
        .expect("front matter");
        assert_eq!(fields.get("name").map(String::as_str), Some("rules"));
        assert_eq!(
            fields.get("description").map(String::as_str),
            Some("before --- after"),
            "the fenced rule belongs to the value, not to the document"
        );
    }

    #[test]
    fn an_unterminated_header_is_not_front_matter() {
        assert!(parse_front_matter("---\nname: stray\nno closing fence\n").is_none());
    }

    #[test]
    fn the_invocation_gate_fails_closed() {
        let root = library(
            "gate",
            &[
                (
                    "shouty",
                    "---\nname: shouty\ndescription: X.\ndisable-model-invocation: TRUE\n---\nB.\n",
                ),
                (
                    "numeric",
                    "---\nname: numeric\ndescription: X.\ndisable-model-invocation: 1\n---\nB.\n",
                ),
                ("open", "---\nname: open\ndescription: X.\n---\nB.\n"),
            ],
        );

        let docs = Docs::load(Some(&root));
        let index = docs.index().expect("one document is still invocable");
        assert!(index.contains("- open:"), "{index}");
        assert!(
            !index.contains("- shouty:") && !index.contains("- numeric:"),
            "an unrecognised truthy value must not leave a skill open: {index}"
        );
        assert_eq!(docs.len(), 3, "all three are still reachable as /name");
    }

    #[test]
    fn a_file_without_front_matter_is_not_a_document() {
        assert!(parse_front_matter("# Just markdown\n").is_none());
    }

    #[test]
    fn missing_directories_are_notes_not_errors() {
        let docs = Docs::load(None);
        assert!(docs.is_empty());
        assert!(docs.notes.is_empty(), "nothing configured, nothing to say");

        let docs = Docs::load(Some(Path::new("/nonexistent/rigagent-skills")));
        assert!(docs.is_empty());
        assert_eq!(docs.notes.len(), 1, "a missing path is reported, not fatal");
    }

    #[test]
    fn documents_load_and_the_index_lists_them() {
        let root = library(
            "index",
            &[
                (
                    "tdd",
                    "---\nname: tdd\ndescription: Test-driven development.\n---\nThe body.\n",
                ),
                (
                    "grilling",
                    "---\nname: grilling\ndescription: Stress-test a plan.\n---\nGrill.\n",
                ),
            ],
        );

        let docs = Docs::load(Some(&root));
        assert_eq!(docs.len(), 2);

        let index = docs.index().expect("an index for two documents");
        assert!(index.contains("- tdd: Test-driven development."));
        assert!(index.contains("- grilling: Stress-test a plan."));
        assert!(
            !index.contains("The body."),
            "bodies must not reach the preamble"
        );

        assert_eq!(docs.body("tdd").expect("body").trim_end(), {
            "---\nname: tdd\ndescription: Test-driven development.\n---\nThe body."
        });
    }

    #[test]
    fn a_malformed_document_is_reported_and_the_rest_still_load() {
        let root = library(
            "malformed",
            &[
                ("good", "---\nname: good\ndescription: Fine.\n---\nBody.\n"),
                ("bad", "no front matter here\n"),
            ],
        );

        let docs = Docs::load(Some(&root));
        assert_eq!(docs.len(), 1, "the good document still loads");
        assert!(
            docs.notes.iter().any(|note| note.contains("bad")),
            "the bad one is reported: {:?}",
            docs.notes
        );
    }

    #[test]
    fn an_unknown_name_answers_with_the_known_ones() {
        let root = library(
            "unknown",
            &[(
                "teach",
                "---\nname: teach\ndescription: Explain.\n---\nBody.\n",
            )],
        );
        let docs = Docs::load(Some(&root));

        let err = docs.body("teech").expect_err("a typo should not resolve");
        let message = err.to_string();
        assert!(message.contains("teech"), "{message}");
        assert!(
            message.contains("teach"),
            "the model needs the real name to retry: {message}"
        );
    }

    #[test]
    fn the_index_is_absent_when_there_is_nothing_to_list() {
        assert!(Docs::default().index().is_none());
    }
}
