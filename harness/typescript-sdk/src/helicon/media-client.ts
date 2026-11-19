/**
 * Minimal HTTP client for the Helicon Search multimedia API.
 *
 * The same tenant credential as the memory client is sent: without it HeliconSearch treats a call as
 * anonymous, which sees the public libraries only. Responses are `{data, retcode, retmsg}`.
 */

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface KbInfo {
  name: string;
  id: string;
  create_date?: string;
  description?: string;
  indexing_mode?: number;
  language?: string;
  filenum?: number;
  embed_id?: string;
  vlm_id?: string;
  video_clip_id?: string;
  audio_clip_id?: string;
  ocr_id?: string;
  asr_id?: string;
  rerank_model?: string;
}

export interface SearchResultItem {
  kb_id: string;
  doc_id: string;
  segment_id: number;
  file: string;
  image?: string;   // thumbnail / processed image path (smaller than original file)
  info: string;
  similarity: number;
  type: string;
  start: number;
  end: number;
  from: string;
}

export interface HeliconMediaResponse<T = unknown> {
  data: T;
  retcode: number;
  retmsg: string;
}

export interface MediaSearchParams {
  kb_id: string;
  message: string;
  label?: string[];
  search_mode?: number;
  rerank_model?: string;
  maximum_return_files?: number;
  threshold?: {
    image_token_similarity?: number;
    video_token_similarity?: number;
    lowest_similarity?: number;
  };
}

export interface SearchByImageParams {
  kb_id: string;
  ref_image?: string;
  doc_id?: string;
  label?: string[];
  maximum_return_files?: number;
  threshold?: {
    lowest_similarity?: number;
  };
}

export interface EnumerateParams {
  kb_id: string;
  batch_id?: string;
  size?: number;
  page?: number;
}

export interface UploadParams {
  kb_id: string;
  file_names: string[];
  metadata?: Record<string, unknown>;
}

export interface BatchStat {
  batch_id: string;
  doc_count: number;
  ready_count: number;
  batch_status: string;
  cost_seconds?: number;
}

export interface StatisticsResult {
  total: number;
  ready: number;
  inprogress: number;
  failure: unknown[];
  batch_lst?: BatchStat[];
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

export class HeliconMediaClient {
  private readonly baseUrl: string;
  private readonly timeoutMs: number;
  private readonly apiKey: string | undefined;

  constructor(cfg: { baseUrl: string; timeoutMs?: number; apiKey?: string }) {
    this.baseUrl = cfg.baseUrl.replace(/\/+$/, "");
    this.timeoutMs = cfg.timeoutMs ?? 30_000;
    this.apiKey = cfg.apiKey;
  }

  private async post<T>(path: string, body: Record<string, unknown>): Promise<HeliconMediaResponse<T>> {
    const url = `${this.baseUrl}${path}`;
    // Same tenant credential the memory client sends (see helicon-client.ts
    // buildHeaders).  Without it the server treats this as anonymous, which
    // resolves to the public resources only — so a tenant's own libraries
    // would silently go missing rather than fail loudly.
    const headers: Record<string, string> = { "Content-Type": "application/json" };
    if (this.apiKey) {
      headers["Authorization"] = `Bearer ${this.apiKey}`;
    }
    const res = await fetch(url, {
      method: "POST",
      headers,
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(this.timeoutMs),
    });

    if (!res.ok) {
      throw new Error(`Helicon Media API ${path} HTTP ${res.status}: ${res.statusText}`);
    }

    return (await res.json()) as HeliconMediaResponse<T>;
  }

  /** List all media libraries (knowledge bases). */
  async listLibraries(): Promise<HeliconMediaResponse<{ kb_list: KbInfo[] }>> {
    return this.post<{ kb_list: KbInfo[] }>("/v1/kb/list", {});
  }

  /** Text-based semantic search in a single KB. */
  async search(params: MediaSearchParams): Promise<HeliconMediaResponse<{ data_list: SearchResultItem[] }>> {
    const body: Record<string, unknown> = {
      kb_id: params.kb_id,
      message: params.message,
      label: params.label ?? ["image", "video"],
      search_mode: params.search_mode ?? 0,
      rerank_model: params.rerank_model ?? "BGE",
      maximum_return_files: params.maximum_return_files ?? 10,
    };
    if (params.threshold) {
      body.threshold = params.threshold;
    }
    return this.post<{ data_list: SearchResultItem[] }>("/v1/document/search", body);
  }

  /** Find similar media by reference image. */
  async searchByImage(params: SearchByImageParams): Promise<HeliconMediaResponse<{ data_list: SearchResultItem[] }>> {
    const body: Record<string, unknown> = {
      kb_id: params.kb_id,
      label: params.label ?? ["image", "video"],
      maximum_return_files: params.maximum_return_files ?? 10,
    };
    if (params.ref_image) {
      body.ref_image = params.ref_image;
    }
    if (params.doc_id) {
      body.doc_id = params.doc_id;
    }
    if (params.threshold) {
      body.threshold = params.threshold;
    }
    return this.post<{ data_list: SearchResultItem[] }>("/v1/document/searchbyimage", body);
  }

  /** Enumerate (paginate) media files in a library. */
  async enumerate(params: EnumerateParams): Promise<HeliconMediaResponse<{
    items: Array<{ file: string; image_path: string; status: string }>;
    total: number;
  }>> {
    const body: Record<string, unknown> = {
      kb_id: params.kb_id,
      size: params.size ?? 50,
      page: params.page ?? 1,
    };
    if (params.batch_id) {
      body.batch_id = params.batch_id;
    }
    return this.post("/v1/document/enumerate", body);
  }

  /** Upload media files to a library for indexing. */
  /** The library this tenant should write to; the server creates or rolls one as needed. */
  async resolveDefaultKb(): Promise<HeliconMediaResponse<{
    kb_id: string;
    name: string;
    created: boolean;
    doc_count: number;
  }>> {
    // Replaces the old "list libraries and take the first" fallback. /v1/kb/list has no
    // ORDER BY, so with more than one library that pick was unstable between calls —
    // and a client choosing where to write bypasses the rule that ownership is decided
    // server-side, from the credential, nowhere else.
    return this.post("/v1/kb/resolve_default", {});
  }

  async upload(params: UploadParams): Promise<HeliconMediaResponse<Record<string, never>>> {
    const body: Record<string, unknown> = {
      kb_id: params.kb_id,
      file_names: params.file_names,
    };
    if (params.metadata) {
      body.metadata = params.metadata;
    }
    return this.post("/v1/document/upload", body);
  }

  /** Query indexing statistics for a library. */
  async statistics(kbId?: string): Promise<HeliconMediaResponse<StatisticsResult>> {
    const body: Record<string, unknown> = {};
    if (kbId) {
      body.kb_id = kbId;
    }
    return this.post<StatisticsResult>("/v1/document/statistics", body);
  }
}
