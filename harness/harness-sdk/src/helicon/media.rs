//! The media tools: search and upload over the tenant's image and video libraries (HeliconSearch's
//! multimedia API, `/v1/kb/*` and `/v1/document/*`). The Rust counterpart of `media-tools.ts` of the
//! TypeScript adapter.
//!
//! ```text
//! media_search           natural-language search over all of the tenant's libraries
//! media_upload           index files into a library
//! media_search_by_image  (mediaAdminTools) find similar media by a reference image
//! media_libraries        (mediaAdminTools) list the libraries
//! media_files            (mediaAdminTools) list the files of a library
//! media_statistics       (mediaAdminTools) indexing progress
//! ```
//!
//! Differences from the TypeScript adapter, both because of what the SDK hands an extension:
//! `media_upload` needs file paths (an extension sees the user's text of a turn, not the images attached to
//! it), and `mediaInlineImages` is not supported (the result of an application tool cannot carry an image).
//! A result is a small JSON object the model reads and an application can render: `results[]` with the file
//! path, the type, the similarity and, for a video, its time span. A failure is a text, as with the memory
//! tools.
//!
//! Paths are paths on the machine HeliconSearch runs on: it reads the files itself, so a library can only be
//! fed files HeliconSearch can open.
use super::{
    client::{HeliconClient, HsError, HsResult},
    config::MemoryConfig,
    log::Logger,
    tools::{capped, MAX_TOOL_OUTPUT_BYTES},
    util::cut_bytes,
};
use crate::Tool;
use futures_util::future::join_all;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const TAG: &str = "[helicon-memory]";
/// Most results one search may ask for.
const MAX_RESULTS: f64 = 50.0;
/// Most files a page of `media_files` lists.
const MAX_PAGE_SIZE: f64 = 200.0;
const DEFAULT_PAGE_SIZE: f64 = 50.0;

pub(crate) struct MediaTools {
    cfg: Arc<MemoryConfig>,
    client: HeliconClient,
    logger: Logger,
}

#[derive(Clone, Debug, PartialEq)]
struct Hit {
    file: String,
    kind: String,
    similarity: f64,
    start: f64,
    end: f64,
    /// A thumbnail or processed image, smaller than the file itself.
    image: String,
}

fn hit_of(item: &Value) -> Option<Hit> {
    let file = item
        .get("file")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty())?;
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let number = |key: &str| item.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    Some(Hit {
        file: file.to_owned(),
        kind: text("type"),
        similarity: number("similarity"),
        start: number("start"),
        end: number("end"),
        image: text("image"),
    })
}

fn hits_of(data: &Value) -> Vec<Hit> {
    data.get("data_list")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(hit_of).collect())
        .unwrap_or_default()
}

/// The best hits of every library, one entry per file (its highest similarity), weak tail dropped.
fn merge_results(batches: Vec<Vec<Hit>>, max: usize) -> Vec<Hit> {
    let mut best: Vec<Hit> = Vec::new();
    for hit in batches.into_iter().flatten() {
        match best.iter_mut().find(|known| known.file == hit.file) {
            Some(known) => {
                if hit.similarity > known.similarity {
                    *known = hit;
                }
            }
            None => best.push(hit),
        }
    }
    best.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if best.len() > 1 {
        // later hits must reach a third of the best one: keeps a long tail of weak matches out
        let floor = best[0].similarity / 3.0;
        best.retain(|h| h.similarity >= floor);
    }
    best.truncate(max);
    best
}

fn hit_json(rank: usize, h: &Hit) -> Value {
    let mut m = Map::new();
    m.insert("rank".into(), json!(rank));
    m.insert("file".into(), json!(h.file));
    m.insert("type".into(), json!(h.kind));
    m.insert(
        "similarity".into(),
        json!((h.similarity * 1000.0).round() / 1000.0),
    );
    if h.kind == "video" {
        m.insert("startSeconds".into(), json!(h.start));
        m.insert("endSeconds".into(), json!(h.end));
    }
    if !h.image.is_empty() && h.image != h.file {
        m.insert("thumbnail".into(), json!(h.image));
    }
    Value::Object(m)
}

fn describe_hits(hits: &[Hit]) -> Value {
    if hits.is_empty() {
        return json!({"message": "No matching images or videos in the media library.", "results": []});
    }
    let results: Vec<Value> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| hit_json(i + 1, h))
        .collect();
    json!({"message": "Matching media (best first).", "results": results})
}

struct Library {
    id: String,
    name: String,
    files: u64,
    language: Option<String>,
}

fn library_of(item: &Value) -> Option<Library> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    // the server writes the count as `fileNum`; a client of an older shape wrote `filenum`
    let files = ["fileNum", "filenum"]
        .iter()
        .find_map(|k| item.get(*k).and_then(Value::as_f64))
        .unwrap_or(0.0);
    Some(Library {
        id: id.to_owned(),
        name: item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        files: files.max(0.0) as u64,
        language: item
            .get("language")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// What a call answers is in `data`; a reply that is not wrapped is the data itself.
fn payload(reply: Value) -> Value {
    match reply {
        Value::Object(mut m) if m.contains_key("data") => m.remove("data").unwrap_or(Value::Null),
        other => other,
    }
}

fn text_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
}

fn number_arg(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn max_results(args: &Value, default: usize) -> usize {
    number_arg(args, "max_results")
        .map_or(default as f64, f64::floor)
        .clamp(1.0, MAX_RESULTS) as usize
}

/// The media types a search names, or `None` for the default (images and videos).
fn labels_arg(args: &Value) -> Option<Vec<String>> {
    let list: Vec<String> = args
        .get("labels")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .collect();
    (!list.is_empty()).then_some(list)
}

fn default_labels() -> Vec<String> {
    vec!["image".to_owned(), "video".to_owned()]
}

fn text(message: impl Into<String>) -> Value {
    Value::String(message.into())
}

/// A result no larger than a tool result may be: the page of a long library would be paid for in the
/// context of every later step.
fn bounded(result: Value) -> Value {
    let rendered = result.to_string();
    if rendered.len() <= MAX_TOOL_OUTPUT_BYTES {
        result
    } else {
        capped(rendered)
    }
}

impl MediaTools {
    pub(crate) fn new(cfg: Arc<MemoryConfig>, client: HeliconClient, logger: Logger) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            client,
            logger,
        })
    }

    fn failed(&self, tool: &str, err: &HsError) -> Value {
        let detail = err.describe();
        self.logger
            .warn(&format!("{TAG} tool={tool} FAILED {detail}"));
        text(format!("{tool} failed: {detail}"))
    }

    async fn post(&self, path: &str, body: Value) -> HsResult<Value> {
        Ok(payload(self.client.media_post(path, body).await?))
    }

    async fn libraries(&self) -> HsResult<Vec<Library>> {
        let data = self.post("/v1/kb/list", json!({})).await?;
        Ok(data
            .get("kb_list")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(library_of).collect())
            .unwrap_or_default())
    }

    /// One search per library, together. The hits of the libraries that answered; the first error when
    /// none did (a library that is down must not read as "nothing found").
    async fn search_all(&self, path: &str, bodies: Vec<Value>) -> HsResult<Vec<Vec<Hit>>> {
        let answers = join_all(bodies.into_iter().map(|body| self.post(path, body))).await;
        let (mut hits, mut first_error) = (Vec::new(), None);
        for answer in answers {
            match answer {
                Ok(data) => hits.push(hits_of(&data)),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            Some(e) if hits.is_empty() => Err(e),
            _ => Ok(hits),
        }
    }

    // ── media_search ───────────────────────────────────────────────────────

    pub(crate) async fn media_search(&self, args: &Value) -> Value {
        let query = text_arg(args, "query");
        if query.is_empty() {
            return text("media_search: query is required");
        }
        let max = max_results(args, self.cfg.media_max_results);
        self.logger.info(&format!(
            "{TAG} tool=media_search query=\"{}\" max={max}",
            cut_bytes(query, 80)
        ));
        let libraries = match self.libraries().await {
            Ok(l) => l,
            Err(e) => return self.failed("media_search", &e),
        };
        if libraries.is_empty() {
            return json!({"message": "media_search: there is no media library yet; use media_upload to add files.", "results": []});
        }
        let lowest = number_arg(args, "lowest_similarity").unwrap_or(self.cfg.media_min_similarity);
        let labels = labels_arg(args).unwrap_or_else(default_labels);
        let bodies = libraries
            .iter()
            .map(|kb| {
                let mut body = json!({
                    "kb_id": kb.id,
                    "message": query,
                    "label": labels,
                    "search_mode": self.cfg.media_search_mode,
                    "rerank_model": self.cfg.media_rerank_model,
                    // room for what de-duplication removes
                    "maximum_return_files": max * 2,
                });
                if lowest > 0.0 {
                    body["threshold"] = json!({"lowest_similarity": lowest});
                }
                body
            })
            .collect();
        match self.search_all("/v1/document/search", bodies).await {
            Ok(batches) => {
                let merged = merge_results(batches, max);
                self.logger.info(&format!(
                    "{TAG} tool=media_search libraries={} results={}",
                    libraries.len(),
                    merged.len()
                ));
                bounded(describe_hits(&merged))
            }
            Err(e) => self.failed("media_search", &e),
        }
    }

    // ── media_upload ───────────────────────────────────────────────────────

    pub(crate) async fn media_upload(&self, args: &Value) -> Value {
        let files: Vec<String> = args
            .get("file_names")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .filter(|f| !f.trim().is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if files.is_empty() {
            return text("media_upload: file_names is required: the absolute paths of the files to add, on the machine HeliconSearch runs on. An image that was only attached to the conversation cannot be added: ask the user for its path.");
        }
        let given = text_arg(args, "kb_id");
        let kb_id = if !given.is_empty() {
            given.to_owned()
        } else if !self.cfg.media_kb_id.is_empty() {
            self.cfg.media_kb_id.clone()
        } else {
            // the server picks (or creates) the tenant's library: ownership is decided from the credential
            match self.post("/v1/kb/resolve_default", json!({})).await {
                Ok(data) => match data.get("kb_id").and_then(Value::as_str) {
                    Some(id) if !id.is_empty() => id.to_owned(),
                    _ => return text(
                        "media_upload: could not resolve a target library: the server named none.",
                    ),
                },
                Err(e) => {
                    return text(format!(
                        "media_upload: could not resolve a target library: {}",
                        e.describe()
                    ))
                }
            }
        };
        self.logger.info(&format!(
            "{TAG} tool=media_upload kb={kb_id} files={}",
            files.len()
        ));
        match self
            .post(
                "/v1/document/upload",
                json!({"kb_id": kb_id, "file_names": files}),
            )
            .await
        {
            Ok(data) => {
                let batch = data
                    .get("batch_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let message = format!(
                    "Submitted {} file(s) for indexing into library {kb_id}{}. They become searchable when indexing finishes.",
                    files.len(),
                    batch
                        .as_deref()
                        .map(|b| format!(" (batch {b})"))
                        .unwrap_or_default()
                );
                bounded(
                    json!({"message": message, "library": kb_id, "batchId": batch, "files": files}),
                )
            }
            Err(e) => self.failed("media_upload", &e),
        }
    }

    // ── the admin tools ────────────────────────────────────────────────────

    pub(crate) async fn media_search_by_image(&self, args: &Value) -> Value {
        let (reference, doc_id) = (text_arg(args, "ref_image"), text_arg(args, "doc_id"));
        if reference.is_empty() && doc_id.is_empty() {
            return text("media_search_by_image: either ref_image or doc_id is required.");
        }
        let max = max_results(args, self.cfg.media_max_results);
        let libraries = match self.libraries().await {
            Ok(l) => l,
            Err(e) => return self.failed("media_search_by_image", &e),
        };
        if libraries.is_empty() {
            return json!({"message": "media_search_by_image: no media libraries found.", "results": []});
        }
        let bodies = libraries
            .iter()
            .map(|kb| {
                let mut body = json!({
                    "kb_id": kb.id,
                    "label": default_labels(),
                    "maximum_return_files": max * 2,
                });
                if !reference.is_empty() {
                    body["ref_image"] = json!(reference);
                }
                if !doc_id.is_empty() {
                    body["doc_id"] = json!(doc_id);
                }
                if let Some(lowest) = number_arg(args, "lowest_similarity") {
                    body["threshold"] = json!({"lowest_similarity": lowest});
                }
                body
            })
            .collect();
        match self.search_all("/v1/document/searchbyimage", bodies).await {
            Ok(batches) => bounded(describe_hits(&merge_results(batches, max))),
            Err(e) => self.failed("media_search_by_image", &e),
        }
    }

    pub(crate) async fn media_libraries(&self) -> Value {
        match self.libraries().await {
            Ok(libraries) => {
                let listed: Vec<Value> = libraries
                    .iter()
                    .map(|l| json!({"id": l.id, "name": l.name, "files": l.files, "language": l.language}))
                    .collect();
                let message = if libraries.is_empty() {
                    "No media libraries found.".to_owned()
                } else {
                    format!("{} media library(ies).", libraries.len())
                };
                bounded(json!({"message": message, "libraries": listed}))
            }
            Err(e) => self.failed("media_libraries", &e),
        }
    }

    pub(crate) async fn media_files(&self, args: &Value) -> Value {
        let given = text_arg(args, "kb_id");
        let kb_id = if given.is_empty() {
            self.cfg.media_kb_id.as_str()
        } else {
            given
        };
        if kb_id.is_empty() {
            return text("media_files: kb_id is required.");
        }
        let page = number_arg(args, "page").map_or(1.0, f64::floor).max(1.0);
        let size = number_arg(args, "size")
            .map_or(DEFAULT_PAGE_SIZE, f64::floor)
            .clamp(1.0, MAX_PAGE_SIZE);
        match self
            .post(
                "/v1/document/enumerate",
                json!({"kb_id": kb_id, "page": page as u64, "size": size as u64}),
            )
            .await
        {
            Ok(data) => {
                let items = data.get("items").cloned().unwrap_or_else(|| json!([]));
                let total = data.get("total").and_then(Value::as_u64).unwrap_or(0);
                let shown = items.as_array().map_or(0, Vec::len);
                let message = if shown == 0 {
                    "No files found.".to_owned()
                } else {
                    format!("Total {total}; showing {shown}.")
                };
                bounded(json!({"message": message, "total": total, "items": items}))
            }
            Err(e) => self.failed("media_files", &e),
        }
    }

    pub(crate) async fn media_statistics(&self, args: &Value) -> Value {
        let given = text_arg(args, "kb_id");
        let kb_id = if given.is_empty() {
            self.cfg.media_kb_id.as_str()
        } else {
            given
        };
        let body = if kb_id.is_empty() {
            json!({})
        } else {
            json!({"kb_id": kb_id})
        };
        match self.post("/v1/document/statistics", body).await {
            Ok(data) => {
                let count = |key: &str| data.get(key).and_then(Value::as_u64).unwrap_or(0);
                let failures = data
                    .get("failure")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let (total, ready, in_progress) =
                    (count("total"), count("ready"), count("inprogress"));
                let message = format!(
                    "Media statistics: total {total}, ready {ready}, in progress {in_progress}{}",
                    if failures > 0 {
                        format!(", failures {failures}")
                    } else {
                        String::new()
                    }
                );
                json!({"message": message, "total": total, "ready": ready, "inProgress": in_progress, "failures": failures})
            }
            Err(e) => self.failed("media_statistics", &e),
        }
    }
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

pub(crate) fn build_tools(media: &Arc<MediaTools>) -> Vec<Tool> {
    // (The text of a tool must not depend on the configuration: the engine keeps the descriptor of a tool name
    // and refuses a changed one, which would switch the extension off at the next start.)
    let mut out: Vec<Tool> = Vec::new();
    if !media.cfg.auto_media {
        return out;
    }

    let m = media.clone();
    out.push(
        Tool::new("media_search")
            .description("Search the user's personal media library for EXISTING images and videos with natural language — use it whenever the user wants to FIND, RETRIEVE or LOOK UP photos or videos they already have (e.g. 'find a photo of...', '找一张...的图片'). Never use an image-generation tool to find existing content. Pass the user's words as they are, without translating or rephrasing. Returns files with similarity and, for video, the time span; give the user the file paths of the best matches.")
            .input_schema(schema(
                json!({
                    "query": {"type": "string", "description": "What the image or video shows, in the user's own words."},
                    "max_results": {"type": "number", "description": "Most results to return (a default is used when omitted)."},
                    "labels": {"type": "array", "items": {"type": "string"}, "description": "Media types to search: \"image\", \"video\" (default both)."},
                    "lowest_similarity": {"type": "number", "description": "Minimum similarity 0-1; weak matches below it are dropped."},
                }),
                &["query"],
            ))
            .handler(move |args, _ctx| {
                let m = m.clone();
                async move { Ok(m.media_search(&args).await) }
            }),
    );

    let m = media.clone();
    out.push(
        Tool::new("media_upload")
            .description("Add image or video files to the media library so they can be found later with media_search. Give file_names as absolute paths of FILES on the machine HeliconSearch runs on. Add a whole directory only when the user explicitly names that directory; never guess a directory to add when a file path does not work — tell the user instead. An image that was only attached to the conversation cannot be added: ask the user for its path. Indexing takes a little while; the files are searchable once it finishes.")
            .input_schema(schema(
                json!({
                    "file_names": {"type": "array", "items": {"type": "string"}, "description": "Absolute file paths (a directory only if the user named it)."},
                    "kb_id": {"type": "string", "description": "Target library id. Omit to use the default library."},
                }),
                &["file_names"],
            ))
            .handler(move |args, _ctx| {
                let m = m.clone();
                async move { Ok(m.media_upload(&args).await) }
            }),
    );

    if !media.cfg.media_admin_tools {
        return out;
    }

    let m = media.clone();
    out.push(
        Tool::new("media_search_by_image")
            .description("Find media that looks like a reference image: give its path on the HeliconSearch machine, or the doc_id of an earlier result.")
            .input_schema(schema(
                json!({
                    "ref_image": {"type": "string", "description": "Absolute path of the reference image. One of ref_image or doc_id is required."},
                    "doc_id": {"type": "string", "description": "doc_id of an earlier result."},
                    "max_results": {"type": "number", "description": "Most results to return (a default is used when omitted)."},
                    "lowest_similarity": {"type": "number", "description": "Minimum similarity 0-1."},
                }),
                &[],
            ))
            .handler(move |args, _ctx| {
                let m = m.clone();
                async move { Ok(m.media_search_by_image(&args).await) }
            }),
    );

    let m = media.clone();
    out.push(
        Tool::new("media_libraries")
            .description("List the media libraries with their ids and file counts.")
            .input_schema(schema(json!({}), &[]))
            .handler(move |_args, _ctx| {
                let m = m.clone();
                async move { Ok(m.media_libraries().await) }
            }),
    );

    let m = media.clone();
    out.push(
        Tool::new("media_files")
            .description("List the files of a media library with their indexing status, a page at a time.")
            .input_schema(schema(
                json!({
                    "kb_id": {"type": "string", "description": "Library id (from media_libraries)."},
                    "page": {"type": "number", "description": "Page number, from 1."},
                    "size": {"type": "number", "description": "Items per page (default 50)."},
                }),
                &[],
            ))
            .handler(move |args, _ctx| {
                let m = m.clone();
                async move { Ok(m.media_files(&args).await) }
            }),
    );

    let m = media.clone();
    out.push(
        Tool::new("media_statistics")
            .description("Indexing progress of a media library: files in total, ready to search, still being indexed.")
            .input_schema(schema(
                json!({"kb_id": {"type": "string", "description": "Library id. Omit for all libraries."}}),
                &[],
            ))
            .handler(move |args, _ctx| {
                let m = m.clone();
                async move { Ok(m.media_statistics(&args).await) }
            }),
    );
    out
}

/// The short names of the tools `build_tools` makes for this configuration.
pub(crate) fn tool_names(cfg: &MemoryConfig) -> Vec<&'static str> {
    if !cfg.auto_media {
        return Vec::new();
    }
    let mut names = vec!["media_search", "media_upload"];
    if cfg.media_admin_tools {
        names.extend([
            "media_search_by_image",
            "media_libraries",
            "media_files",
            "media_statistics",
        ]);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helicon::test_support::{MockHs, Reply};

    fn media(data: Value) -> Reply {
        Reply::ok(json!({"data": data, "retcode": 0, "retmsg": "success"}))
    }

    fn refused(message: &str) -> Reply {
        Reply::ok(json!({"data": null, "retcode": 1, "retmsg": message}))
    }

    fn tools(hs: &MockHs, extra: Value) -> Arc<MediaTools> {
        let mut raw = json!({"baseUrl": hs.url(), "apiKey": "hs_test"});
        for (k, v) in extra.as_object().unwrap() {
            raw[k] = v.clone();
        }
        let cfg = Arc::new(MemoryConfig::parse(&raw, &|_| None).unwrap());
        let logger = Logger::new(std::env::temp_dir().join("helicon-media-test.log"), false);
        let client = HeliconClient::new(&cfg.base_url, &cfg.api_key, 2_000, logger.clone(), false);
        MediaTools::new(cfg, client, logger)
    }

    fn hit(file: &str, similarity: f64) -> Hit {
        Hit {
            file: file.into(),
            kind: "image".into(),
            similarity,
            start: 0.0,
            end: 0.0,
            image: String::new(),
        }
    }

    fn files(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.file.as_str()).collect()
    }

    #[test]
    fn results_merge_per_file_and_drop_a_weak_tail() {
        let merged = merge_results(
            vec![
                vec![hit("/a.jpg", 0.9), hit("/b.jpg", 0.2)],
                vec![hit("/a.jpg", 0.5), hit("/c.jpg", 0.7), hit("/d.jpg", 0.1)],
            ],
            5,
        );
        // /a.jpg keeps its best 0.9; the floor is a third of it (0.3): /b.jpg and /d.jpg are the weak tail
        assert_eq!(files(&merged), ["/a.jpg", "/c.jpg"]);
        assert_eq!(merged[0].similarity, 0.9);
        let capped_at_one = merge_results(vec![vec![hit("/a.jpg", 0.9), hit("/c.jpg", 0.8)]], 1);
        assert_eq!(files(&capped_at_one), ["/a.jpg"]);
        // a lone weak hit is still a hit: the floor only trims a tail behind a better one
        assert_eq!(
            files(&merge_results(vec![vec![hit("/x.jpg", 0.01)]], 5)),
            ["/x.jpg"]
        );
        assert!(merge_results(vec![], 5).is_empty());
    }

    #[test]
    fn the_tool_names_follow_the_options() {
        let cfg = |extra: Value| {
            let mut raw = json!({"baseUrl": "http://127.0.0.1:5001", "apiKey": "k"});
            for (k, v) in extra.as_object().unwrap() {
                raw[k] = v.clone();
            }
            MemoryConfig::parse(&raw, &|_| None).unwrap()
        };
        assert_eq!(
            tool_names(&cfg(json!({}))),
            vec!["media_search", "media_upload"]
        );
        assert_eq!(tool_names(&cfg(json!({"mediaAdminTools": true}))).len(), 6);
        assert!(tool_names(&cfg(json!({"autoMedia": false}))).is_empty());
        assert!(tool_names(&cfg(json!({"autoMedia": false, "mediaAdminTools": true}))).is_empty());
    }

    #[tokio::test]
    async fn search_asks_every_library_merges_and_answers_with_hits() {
        let hs = MockHs::start(|req| {
            assert_eq!(req.method, "POST");
            assert_eq!(
                req.auth.as_deref(),
                Some("Bearer hs_test"),
                "the tenant credential goes with every media call"
            );
            match req.path.as_str() {
                "/v1/kb/list" => media(json!({"kb_list": [
                    {"id": "kb1", "name": "photos", "fileNum": 3},
                    {"id": "kb2", "name": "videos", "fileNum": 1},
                ]})),
                "/v1/document/search" => {
                    assert_eq!(req.body["message"], "a red car");
                    assert_eq!(req.body["label"], json!(["image", "video"]));
                    assert_eq!(req.body["search_mode"], 2);
                    assert_eq!(req.body["rerank_model"], "BGE");
                    assert_eq!(req.body["maximum_return_files"], 10);
                    assert_eq!(req.body["threshold"]["lowest_similarity"], 0.05);
                    if req.body["kb_id"] == "kb1" {
                        media(json!({"data_list": [{"file": "/p/a.jpg", "type": "image", "similarity": 0.91234,
                            "image": "/thumb/a.jpg", "start": 0, "end": 0}]}))
                    } else {
                        media(json!({"data_list": [{"file": "/p/v.mp4", "type": "video", "similarity": 0.5,
                            "image": "/p/v.mp4", "start": 3.0, "end": 7.5}]}))
                    }
                }
                other => panic!("unexpected {other}"),
            }
        })
        .await;
        let t = tools(&hs, json!({}));
        let out = t
            .media_search(&json!({"query": "a red car", "max_results": 5}))
            .await;
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "{out}");
        assert_eq!(results[0]["rank"], 1);
        assert_eq!(results[0]["file"], "/p/a.jpg");
        assert_eq!(results[0]["similarity"], 0.912);
        assert_eq!(results[0]["thumbnail"], "/thumb/a.jpg");
        assert_eq!(results[1]["type"], "video");
        assert_eq!(results[1]["startSeconds"], 3.0);
        assert_eq!(results[1]["endSeconds"], 7.5);
        assert!(
            results[1].get("thumbnail").is_none(),
            "a thumbnail that is the file itself is not one"
        );
        assert_eq!(
            hs.requests().len(),
            3,
            "the libraries, then one search each"
        );
    }

    #[tokio::test]
    async fn search_options_reach_the_server_and_the_limits_hold() {
        let hs = MockHs::start(|req| match req.path.as_str() {
            "/v1/kb/list" => media(json!({"kb_list": [{"id": "kb1", "name": "p"}]})),
            "/v1/document/search" => {
                assert_eq!(req.body["label"], json!(["video"]));
                assert_eq!(req.body["maximum_return_files"], 100, "50 at most, twice");
                assert!(
                    req.body.get("threshold").is_none(),
                    "no threshold when none is asked for: {}",
                    req.body
                );
                assert_eq!(req.body["search_mode"], 1);
                assert_eq!(req.body["rerank_model"], "M");
                media(json!({"data_list": []}))
            }
            other => panic!("unexpected {other}"),
        })
        .await;
        let t = tools(
            &hs,
            json!({"mediaSearchMode": 1, "mediaRerankModel": "M", "mediaMinSimilarity": 0}),
        );
        let out = t
            .media_search(&json!({"query": "x", "max_results": 500, "labels": ["video", " "]}))
            .await;
        assert_eq!(
            out["message"],
            "No matching images or videos in the media library."
        );
        assert_eq!(out["results"], json!([]));
    }

    #[tokio::test]
    async fn search_says_in_words_when_it_cannot_search() {
        let none = MockHs::start(|_| media(json!({"kb_list": []}))).await;
        let out = tools(&none, json!({}))
            .media_search(&json!({"query": "x"}))
            .await;
        assert!(out["message"]
            .as_str()
            .unwrap()
            .contains("no media library yet"));
        assert_eq!(none.requests().len(), 1);

        let refuses = MockHs::start(|_| refused("nope")).await;
        let out = tools(&refuses, json!({}))
            .media_search(&json!({"query": "x"}))
            .await;
        let said = out.as_str().unwrap();
        assert!(
            said.starts_with("media_search failed:") && said.contains("nope"),
            "{said}"
        );

        // every library's search failing is a failure, not "nothing found"
        let down = MockHs::start(|req| match req.path.as_str() {
            "/v1/kb/list" => media(json!({"kb_list": [{"id": "kb1"}, {"id": "kb2"}]})),
            _ => Reply::status(500, json!({"message": "boom"})),
        })
        .await;
        let out = tools(&down, json!({}))
            .media_search(&json!({"query": "x"}))
            .await;
        assert!(
            out.as_str()
                .is_some_and(|s| s.starts_with("media_search failed:")),
            "{out}"
        );

        // one library failing among others that answered is not
        let partly = MockHs::start(|req| match req.path.as_str() {
            "/v1/kb/list" => media(json!({"kb_list": [{"id": "kb1"}, {"id": "kb2"}]})),
            _ if req.body["kb_id"] == "kb1" => media(
                json!({"data_list": [{"file": "/a.jpg", "type": "image", "similarity": 0.8}]}),
            ),
            _ => Reply::status(500, json!({"message": "boom"})),
        })
        .await;
        let out = tools(&partly, json!({}))
            .media_search(&json!({"query": "x"}))
            .await;
        assert_eq!(out["results"][0]["file"], "/a.jpg", "{out}");

        let bare = tools(&none, json!({}));
        assert_eq!(
            bare.media_search(&json!({"query": "  "})).await,
            "media_search: query is required"
        );
    }

    #[tokio::test]
    async fn upload_adds_the_files_to_the_library_the_server_names() {
        let hs = MockHs::start(|req| match req.path.as_str() {
            "/v1/kb/resolve_default" => {
                media(json!({"kb_id": "kb1", "name": "photos", "created": false, "doc_count": 3}))
            }
            "/v1/document/upload" => {
                assert_eq!(req.body["kb_id"], "kb1");
                assert_eq!(req.body["file_names"], json!(["/p/a.jpg", "/p/b.png"]));
                media(json!({"batch_id": "b1"}))
            }
            other => panic!("unexpected {other}"),
        })
        .await;
        let out = tools(&hs, json!({}))
            .media_upload(&json!({"file_names": ["/p/a.jpg", "  ", "/p/b.png"]}))
            .await;
        let message = out["message"].as_str().unwrap();
        assert!(
            message.starts_with("Submitted 2 file(s) for indexing into library kb1 (batch b1)"),
            "{message}"
        );
        assert_eq!(out["library"], "kb1");
        assert_eq!(out["batchId"], "b1");
    }

    #[tokio::test]
    async fn upload_uses_the_library_it_is_told_without_asking_the_server_to_choose() {
        for (extra, args) in [
            (
                json!({}),
                json!({"file_names": ["/a.jpg"], "kb_id": "mine"}),
            ),
            (
                json!({"mediaKbId": "mine"}),
                json!({"file_names": ["/a.jpg"]}),
            ),
        ] {
            let hs = MockHs::start(|req| {
                assert_eq!(req.path, "/v1/document/upload", "no library to resolve");
                assert_eq!(req.body["kb_id"], "mine");
                media(json!({}))
            })
            .await;
            let out = tools(&hs, extra).media_upload(&args).await;
            assert!(
                out["message"].as_str().unwrap().contains("library mine"),
                "{out}"
            );
            assert!(out["batchId"].is_null());
        }
    }

    #[tokio::test]
    async fn upload_without_paths_or_when_refused_answers_in_words() {
        let hs = MockHs::start(|req| match req.path.as_str() {
            "/v1/kb/resolve_default" => media(json!({"kb_id": "kb1"})),
            _ => refused("cannot read /nope.jpg"),
        })
        .await;
        let t = tools(&hs, json!({}));
        let none = t.media_upload(&json!({})).await;
        assert!(
            none.as_str().unwrap().contains("file_names is required")
                && none.as_str().unwrap().contains("ask the user for its path"),
            "{none}"
        );
        assert!(hs.requests().is_empty());
        let out = t.media_upload(&json!({"file_names": ["/nope.jpg"]})).await;
        let said = out.as_str().unwrap();
        assert!(
            said.starts_with("media_upload failed:") && said.contains("cannot read /nope.jpg"),
            "{said}"
        );

        let no_library = MockHs::start(|_| refused("no tenant")).await;
        let out = tools(&no_library, json!({}))
            .media_upload(&json!({"file_names": ["/a.jpg"]}))
            .await;
        assert!(
            out.as_str()
                .unwrap()
                .starts_with("media_upload: could not resolve a target library:"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn search_by_image_needs_a_reference_and_sends_it() {
        let hs = MockHs::start(|req| match req.path.as_str() {
            "/v1/kb/list" => media(json!({"kb_list": [{"id": "kb1"}]})),
            "/v1/document/searchbyimage" => {
                assert_eq!(req.body["ref_image"], "/ref.jpg");
                assert!(req.body.get("doc_id").is_none());
                assert_eq!(req.body["label"], json!(["image", "video"]));
                assert_eq!(req.body["threshold"]["lowest_similarity"], 0.4);
                media(
                    json!({"data_list": [{"file": "/s.jpg", "type": "image", "similarity": 0.7}]}),
                )
            }
            other => panic!("unexpected {other}"),
        })
        .await;
        let t = tools(&hs, json!({"mediaAdminTools": true}));
        assert!(t
            .media_search_by_image(&json!({}))
            .await
            .as_str()
            .unwrap()
            .contains("either ref_image or doc_id"));
        let out = t
            .media_search_by_image(&json!({"ref_image": "/ref.jpg", "lowest_similarity": 0.4}))
            .await;
        assert_eq!(out["results"][0]["file"], "/s.jpg");
    }

    #[tokio::test]
    async fn the_admin_tools_list_libraries_files_and_progress() {
        let hs = MockHs::start(|req| match req.path.as_str() {
            "/v1/kb/list" => media(json!({"kb_list": [
                {"id": "kb1", "name": "photos", "fileNum": 88, "language": "zh"},
                {"id": "kb2", "name": "old", "filenum": 2},
            ]})),
            "/v1/document/enumerate" => {
                assert_eq!(req.body["kb_id"], "kb1");
                assert_eq!(req.body["page"], 1);
                assert_eq!(req.body["size"], 200, "a page is at most 200");
                media(json!({"items": [{"file": "/a.jpg", "status": "ready"}], "total": 88}))
            }
            "/v1/document/statistics" => {
                assert_eq!(req.body, json!({"kb_id": "kb1"}));
                media(json!({"total": 3, "ready": 2, "inprogress": 1, "failure": [{"f": 1}]}))
            }
            other => panic!("unexpected {other}"),
        })
        .await;
        let t = tools(&hs, json!({"mediaAdminTools": true}));
        let libs = t.media_libraries().await;
        assert_eq!(libs["libraries"][0]["files"], 88, "the server's `fileNum`");
        assert_eq!(libs["libraries"][1]["files"], 2, "and the older `filenum`");
        assert_eq!(libs["libraries"][0]["language"], "zh");
        assert_eq!(
            t.media_files(&json!({})).await,
            "media_files: kb_id is required."
        );
        let files = t
            .media_files(&json!({"kb_id": "kb1", "page": 0, "size": 9999}))
            .await;
        assert_eq!(files["total"], 88);
        assert_eq!(files["items"][0]["file"], "/a.jpg");
        let stats = t.media_statistics(&json!({"kb_id": "kb1"})).await;
        assert_eq!(
            stats["message"],
            "Media statistics: total 3, ready 2, in progress 1, failures 1"
        );
        assert_eq!(stats["inProgress"], 1);
    }

    #[tokio::test]
    async fn the_library_of_the_configuration_is_the_default_of_files_and_statistics() {
        let hs = MockHs::start(|req| {
            assert_eq!(req.body["kb_id"], "cfgkb");
            match req.path.as_str() {
                "/v1/document/enumerate" => media(json!({"items": [], "total": 0})),
                "/v1/document/statistics" => {
                    media(json!({"total": 0, "ready": 0, "inprogress": 0}))
                }
                other => panic!("unexpected {other}"),
            }
        })
        .await;
        let t = tools(&hs, json!({"mediaKbId": "cfgkb"}));
        assert_eq!(
            t.media_files(&json!({})).await["message"],
            "No files found."
        );
        assert_eq!(t.media_statistics(&json!({})).await["total"], 0);
    }

    #[test]
    fn a_tool_result_is_bounded_like_any_other() {
        let big = json!({"items": vec!["x".repeat(1000); 100]});
        let out = bounded(big);
        assert!(out.as_str().is_some_and(|s| s.contains("cut here")));
        let small = json!({"items": ["a"]});
        assert_eq!(bounded(small.clone()), small);
    }
}
