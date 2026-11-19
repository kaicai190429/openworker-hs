/**
 * Media (image / video) search and upload tools, ported from the OpenClaw plugin.
 *
 *   media_search           natural-language search over all of the tenant's media libraries
 *   media_upload           index files into a library; with no paths, the images the user attached
 *   media_search_by_image  (mediaAdminTools) find similar media by a reference image
 *   media_libraries        (mediaAdminTools) list libraries
 *   media_files            (mediaAdminTools) list the files of a library
 *   media_statistics       (mediaAdminTools) indexing progress
 *
 * Differences from OpenClaw, which delivered pictures to a chat UI through `MEDIA:` lines: here the
 * result is a structured object (`results[]` with file path, type, similarity, time span) that the
 * application can render itself, and, with `mediaInlineImages`, the best image hit is also shown to
 * the model (one JPEG/PNG, as the SDK's tool images allow) so it can check what it found.
 *
 * Paths are paths on the machine HeliconSearch runs on: it reads the files itself, so a library
 * can only be fed files HeliconSearch can open.
 */
import { readFile, stat } from "node:fs/promises";
import { extname } from "node:path";
import type { MemoryConfig } from "./config.js";
import type { HeliconMediaClient, KbInfo, SearchResultItem } from "./media-client.js";
import type { ToolCtx, ToolHost, ToolResult } from "./tools.js";

const PLUGIN_ID = "helicon-memory";
/** The SDK accepts at most this many base64 characters of image in a tool result. */
const MAX_IMAGE_BASE64 = 700_000;

function result(text: string, data?: unknown, image?: ToolResult["image"]): ToolResult {
  return { content: [{ type: "text", text }], ...(data !== undefined ? { data } : {}), ...(image ? { image } : {}) };
}

const message = (err: unknown): string => (err instanceof Error ? err.message : String(err));

/** The best hits of every library, one entry per file (its highest similarity), weak tail dropped. */
export function mergeResults(batches: SearchResultItem[][], max: number): SearchResultItem[] {
  const byFile = new Map<string, SearchResultItem>();
  for (const item of batches.flat()) {
    const known = byFile.get(item.file);
    if (!known || item.similarity > known.similarity) byFile.set(item.file, item);
  }
  const sorted = [...byFile.values()].sort((a, b) => b.similarity - a.similarity);
  const top = sorted[0];
  if (top && sorted.length > 1) {
    // later hits must reach a third of the best one: keeps a long tail of weak matches out
    const floor = top.similarity / 3;
    return sorted.filter((item) => item.similarity >= floor).slice(0, max);
  }
  return sorted.slice(0, max);
}

type Hit = {
  rank: number;
  file: string;
  type: string;
  similarity: number;
  startSeconds?: number;
  endSeconds?: number;
  thumbnail?: string;
};

function toHits(items: SearchResultItem[]): Hit[] {
  return items.map((item, i) => ({
    rank: i + 1,
    file: item.file,
    type: item.type,
    similarity: Math.round(item.similarity * 1000) / 1000,
    ...(item.type === "video" ? { startSeconds: item.start, endSeconds: item.end } : {}),
    ...(item.image && item.image !== item.file ? { thumbnail: item.image } : {}),
  }));
}

const IMAGE_TYPES: Record<string, "image/jpeg" | "image/png"> = {
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".png": "image/png",
};

/** The best image hit as a model-visible picture, when it is a JPEG/PNG small enough. */
async function topImage(items: SearchResultItem[]): Promise<ToolResult["image"] | undefined> {
  for (const item of items) {
    if (item.type !== "image") continue;
    for (const path of [item.image, item.file]) {
      const mimeType = path ? IMAGE_TYPES[extname(path).toLowerCase()] : undefined;
      if (!path || !mimeType) continue;
      try {
        const info = await stat(path);
        if (!info.isFile() || Math.ceil((info.size * 4) / 3) > MAX_IMAGE_BASE64) continue;
        return { mimeType, base64: (await readFile(path)).toString("base64") };
      } catch {
        /* HeliconSearch's file is not readable from here: try the next candidate */
      }
    }
    return undefined; // only the best image hit is shown
  }
  return undefined;
}

async function describeHits(items: SearchResultItem[], inline: boolean): Promise<ToolResult> {
  if (items.length === 0) return result("No matching images or videos in the media library.", { results: [] });
  const hits = toHits(items);
  const text = [
    "Matching media (best first):",
    ...hits.map((h) =>
      `${h.rank}. ${h.file} [${h.type}, similarity ${h.similarity}]` +
      (h.startSeconds !== undefined ? ` ${h.startSeconds.toFixed(1)}s–${(h.endSeconds ?? 0).toFixed(1)}s` : ""),
    ),
  ].join("\n");
  const image = inline ? await topImage(items) : undefined;
  return result(text, { results: hits, ...(image ? { shownToModel: hits[0]?.file } : {}) }, image);
}

async function librariesOf(client: HeliconMediaClient): Promise<{ kbs: KbInfo[]; error?: string }> {
  const res = await client.listLibraries();
  if (res.retcode !== 0) return { kbs: [], error: `failed to list libraries (retcode ${res.retcode}): ${res.retmsg}` };
  return { kbs: res.data?.kb_list ?? [] };
}

function settledItems(settled: PromiseSettledResult<{ retcode: number; data?: { data_list?: SearchResultItem[] } }>[]): SearchResultItem[][] {
  return settled.flatMap((r) => (r.status === "fulfilled" && r.value.retcode === 0 ? [r.value.data?.data_list ?? []] : []));
}

export function registerMediaTools(params: {
  api: ToolHost;
  client: HeliconMediaClient;
  cfg: MemoryConfig;
  /** the files the user attached in this session (saved by the adapter), oldest first */
  attachedFiles: (sessionId: string) => string[];
}): void {
  const { api, client, cfg, attachedFiles } = params;
  const log = (msg: string): void => api.logger.info(`[${PLUGIN_ID}] ${msg}`);

  api.registerTool({
    name: "media_search",
    label: "Media Search (HeliconSearch)",
    description:
      "Search the user's personal media library for EXISTING images and videos with natural language — use it whenever the " +
      "user wants to FIND, RETRIEVE or LOOK UP photos or videos they already have (e.g. 'find a photo of...', '找一张...的图片'). " +
      "Never use an image-generation tool to find existing content. Pass the user's words as they are, without translating " +
      "or rephrasing. Returns files with similarity and, for video, the time span; give the user the file paths of the best matches.",
    parameters: {
      type: "object",
      required: ["query"],
      additionalProperties: false,
      properties: {
        query: { type: "string", description: "What the image or video shows, in the user's own words." },
        max_results: { type: "number", description: `Most results to return (default ${cfg.mediaMaxResults}).` },
        labels: { type: "array", items: { type: "string" }, description: 'Media types to search: "image", "video" (default both).' },
        lowest_similarity: { type: "number", description: "Minimum similarity 0-1; weak matches below it are dropped." },
      },
    },
    async execute(_id, raw) {
      const p = (raw ?? {}) as { query?: string; max_results?: number; labels?: string[]; lowest_similarity?: number };
      const query = (p.query ?? "").trim();
      if (!query) return result("media_search: query is required");
      const max = Math.max(1, Math.min(50, Math.floor(p.max_results ?? cfg.mediaMaxResults)));
      log(`tool=media_search query="${query.slice(0, 80)}" max=${max}`);
      try {
        const { kbs, error } = await librariesOf(client);
        if (error) return result(`media_search: ${error}`);
        if (kbs.length === 0) return result("media_search: there is no media library yet; use media_upload to add files.", { results: [] });
        const minSim = p.lowest_similarity ?? cfg.mediaMinSimilarity;
        const settled = await Promise.allSettled(
          kbs.map((kb) =>
            client.search({
              kb_id: kb.id,
              message: query,
              ...(p.labels ? { label: p.labels } : {}),
              search_mode: cfg.mediaSearchMode,
              rerank_model: cfg.mediaRerankModel,
              maximum_return_files: max * 2, // room for what de-duplication removes
              ...(minSim > 0 ? { threshold: { lowest_similarity: minSim } } : {}),
            }),
          ),
        );
        const merged = mergeResults(settledItems(settled), max);
        log(`tool=media_search libraries=${kbs.length} results=${merged.length}`);
        return await describeHits(merged, cfg.mediaInlineImages);
      } catch (err) {
        api.logger.warn(`[${PLUGIN_ID}] tool=media_search FAILED: ${message(err)}`);
        return result(`media_search failed: ${message(err)}`);
      }
    },
  });

  api.registerTool((toolCtx: ToolCtx) => ({
    name: "media_upload",
    label: "Media Upload (HeliconSearch)",
    description:
      "Add image or video files to the media library so they can be found later with media_search. Give file_names as absolute " +
      "paths of FILES on the machine HeliconSearch runs on. Add a whole directory only when the user explicitly names that directory; " +
      "never guess a directory to add when a file path does not work — tell the user instead. With no file_names, the images the user " +
      "attached in this conversation are added. Indexing takes a little while; the files are searchable once it finishes.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        file_names: { type: "array", items: { type: "string" }, description: "Absolute file paths (a directory only if the user named it). Omit to add the user's attached images." },
        kb_id: { type: "string", description: "Target library id. Omit to use the default library." },
      },
    },
    async execute(_id, raw) {
      const p = (raw ?? {}) as { file_names?: string[]; kb_id?: string };
      let files = (p.file_names ?? []).filter((f) => typeof f === "string" && f.trim());
      if (files.length === 0) {
        files = attachedFiles(toolCtx.sessionId ?? "");
        if (files.length === 0) {
          return result("media_upload: no file_names given and the user has attached no images in this conversation.");
        }
        log(`tool=media_upload using ${files.length} attached image(s)`);
      }
      try {
        let kbId = p.kb_id || cfg.mediaKbId;
        if (!kbId) {
          // the server picks (or creates) the tenant's library: ownership is decided from the credential
          const res = await client.resolveDefaultKb();
          if (res.retcode !== 0 || !res.data?.kb_id) {
            return result(`media_upload: could not resolve a target library (retcode ${res.retcode}): ${res.retmsg}`);
          }
          kbId = res.data.kb_id;
        }
        log(`tool=media_upload kb=${kbId} files=${files.length}`);
        const res = await client.upload({ kb_id: kbId, file_names: files });
        if (res.retcode !== 0) return result(`media_upload failed (retcode ${res.retcode}): ${res.retmsg}`);
        const batchId = (res.data as { batch_id?: string } | undefined)?.batch_id;
        return result(
          `Submitted ${files.length} file(s) for indexing into library ${kbId}${batchId ? ` (batch ${batchId})` : ""}. ` +
          "They become searchable when indexing finishes.",
          { library: kbId, batchId, files },
        );
      } catch (err) {
        api.logger.warn(`[${PLUGIN_ID}] tool=media_upload FAILED: ${message(err)}`);
        return result(`media_upload failed: ${message(err)}`);
      }
    },
  }), { name: "media_upload" });

  if (!cfg.mediaAdminTools) return;

  api.registerTool({
    name: "media_search_by_image",
    label: "Media Search by Image (HeliconSearch)",
    description: "Find media that looks like a reference image: give its path on the HeliconSearch machine, or the doc_id of an earlier result.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        ref_image: { type: "string", description: "Absolute path of the reference image. One of ref_image or doc_id is required." },
        doc_id: { type: "string", description: "doc_id of an earlier result." },
        max_results: { type: "number", description: `Most results to return (default ${cfg.mediaMaxResults}).` },
        lowest_similarity: { type: "number", description: "Minimum similarity 0-1." },
      },
    },
    async execute(_id, raw) {
      const p = (raw ?? {}) as { ref_image?: string; doc_id?: string; max_results?: number; lowest_similarity?: number };
      if (!p.ref_image && !p.doc_id) return result("media_search_by_image: either ref_image or doc_id is required.");
      const max = Math.max(1, Math.min(50, Math.floor(p.max_results ?? cfg.mediaMaxResults)));
      try {
        const { kbs, error } = await librariesOf(client);
        if (error) return result(`media_search_by_image: ${error}`);
        if (kbs.length === 0) return result("media_search_by_image: no media libraries found.", { results: [] });
        const settled = await Promise.allSettled(
          kbs.map((kb) =>
            client.searchByImage({
              kb_id: kb.id,
              ...(p.ref_image ? { ref_image: p.ref_image } : {}),
              ...(p.doc_id ? { doc_id: p.doc_id } : {}),
              maximum_return_files: max * 2,
              ...(p.lowest_similarity != null ? { threshold: { lowest_similarity: p.lowest_similarity } } : {}),
            }),
          ),
        );
        return await describeHits(mergeResults(settledItems(settled), max), cfg.mediaInlineImages);
      } catch (err) {
        return result(`media_search_by_image failed: ${message(err)}`);
      }
    },
  });

  api.registerTool({
    name: "media_libraries",
    label: "Media Libraries (HeliconSearch)",
    description: "List the media libraries with their ids and file counts.",
    parameters: { type: "object", additionalProperties: false, properties: {} },
    async execute() {
      try {
        const { kbs, error } = await librariesOf(client);
        if (error) return result(`media_libraries: ${error}`);
        const libraries = kbs.map((kb) => ({ id: kb.id, name: kb.name, files: kb.filenum ?? 0, language: kb.language ?? null }));
        return result(
          kbs.length === 0 ? "No media libraries found." : [`${kbs.length} media library(ies):`, ...libraries.map((l) => `- ${l.name} (id: ${l.id}) files: ${l.files}`)].join("\n"),
          { libraries },
        );
      } catch (err) {
        return result(`media_libraries failed: ${message(err)}`);
      }
    },
  });

  api.registerTool({
    name: "media_files",
    label: "Media Files (HeliconSearch)",
    description: "List the files of a media library with their indexing status, a page at a time.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        kb_id: { type: "string", description: "Library id (from media_libraries)." },
        page: { type: "number", description: "Page number, from 1." },
        size: { type: "number", description: "Items per page (default 50)." },
      },
    },
    async execute(_id, raw) {
      const p = (raw ?? {}) as { kb_id?: string; page?: number; size?: number };
      const kbId = p.kb_id || cfg.mediaKbId;
      if (!kbId) return result("media_files: kb_id is required.");
      try {
        const res = await client.enumerate({ kb_id: kbId, ...(p.page !== undefined ? { page: p.page } : {}), ...(p.size !== undefined ? { size: p.size } : {}) });
        if (res.retcode !== 0) return result(`media_files failed (retcode ${res.retcode}): ${res.retmsg}`);
        const items = res.data?.items ?? [];
        return result(
          items.length === 0 ? "No files found." : [`Total ${res.data?.total ?? 0}; showing ${items.length}:`, ...items.map((i) => `- ${i.file} [${i.status}]`)].join("\n"),
          { total: res.data?.total ?? 0, items },
        );
      } catch (err) {
        return result(`media_files failed: ${message(err)}`);
      }
    },
  });

  api.registerTool({
    name: "media_statistics",
    label: "Media Statistics (HeliconSearch)",
    description: "Indexing progress of a media library: files in total, ready to search, still being indexed.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: { kb_id: { type: "string", description: "Library id. Omit for all libraries." } },
    },
    async execute(_id, raw) {
      const p = (raw ?? {}) as { kb_id?: string };
      try {
        const res = await client.statistics(p.kb_id || cfg.mediaKbId || undefined);
        if (res.retcode !== 0) return result(`media_statistics failed (retcode ${res.retcode}): ${res.retmsg}`);
        const d = res.data;
        return result(
          `Media statistics: total ${d.total}, ready ${d.ready}, in progress ${d.inprogress}` + (d.failure?.length ? `, failures ${d.failure.length}` : ""),
          { total: d.total, ready: d.ready, inProgress: d.inprogress, failures: d.failure?.length ?? 0 },
        );
      } catch (err) {
        return result(`media_statistics failed: ${message(err)}`);
      }
    },
  });
}
