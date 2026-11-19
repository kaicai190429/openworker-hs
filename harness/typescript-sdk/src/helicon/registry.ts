/**
 * Which harness has an adapter attached. Kept beside the harness (not on it) so the public `Harness`
 * class gains no member: the SDK's classes just ask `adapterOf(harness)` at the few points that matter.
 */
import type { HeliconAdapter } from "./adapter.js";

const attached = new WeakMap<object, HeliconAdapter>();

export function setAdapter(harness: object, adapter: HeliconAdapter): void {
  attached.set(harness, adapter);
}

export function adapterOf(harness: object): HeliconAdapter | undefined {
  return attached.get(harness);
}

export type ImageAttachment = { name: string; mediaType: string; bytes: Buffer };

const IMAGE_EXT: Record<string, string> = { "image/png": ".png", "image/jpeg": ".jpg", "image/gif": ".gif", "image/webp": ".webp" };
export const imageExtension = (mediaType: string): string => IMAGE_EXT[mediaType] ?? ".img";

/** The images a message input carries (attached files or inline base64); staged ids cannot be read back. */
export function imageAttachments(input: unknown): ImageAttachment[] {
  const content = (input as { content?: unknown } | null)?.content;
  if (!Array.isArray(content)) return [];
  const out: ImageAttachment[] = [];
  for (const block of content) {
    const b = block as { type?: unknown; name?: unknown; mediaType?: unknown; bytes?: unknown; source?: { type?: unknown; mediaType?: unknown; data?: unknown } };
    if (typeof b?.bytes === "function" && b.type === "image" && typeof b.mediaType === "string") {
      out.push({ name: String(b.name ?? "image"), mediaType: b.mediaType, bytes: (b.bytes as () => Buffer)() });
    } else if (b?.type === "image" && b.source?.type === "base64" && typeof b.source.data === "string" && typeof b.source.mediaType === "string") {
      out.push({ name: String(b.name ?? "image"), mediaType: b.source.mediaType, bytes: Buffer.from(b.source.data, "base64") });
    }
  }
  return out;
}

/** The text of a message input (what the user said), for recall. Attachments contribute nothing. */
export function messageText(input: unknown): string {
  if (typeof input === "string") return input;
  if (!input || typeof input !== "object") return "";
  const value = input as { text?: unknown; content?: unknown; type?: unknown };
  if (value.type === "approval" || value.type === "answer") return "";
  if (typeof value.text === "string") return value.text;
  if (Array.isArray(value.content)) {
    return value.content
      .map((block) => {
        const b = block as { type?: unknown; text?: unknown };
        return b?.type === "text" && typeof b.text === "string" ? b.text : "";
      })
      .filter(Boolean)
      .join("\n");
  }
  return "";
}
