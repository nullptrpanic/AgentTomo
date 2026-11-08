import { previewContent } from "./file-preview.mjs";

export const contentEncoding = headers => (headers || []).filter(([name]) => name.toLowerCase() === "content-encoding").map(([, value]) => value).join(", ");

export function httpPreview(payload, headers, body, raw = false) {
  const encoding = contentEncoding(headers);
  const type = (headers || []).find(([name]) => name.toLowerCase() === "content-type")?.[1] || "";
  const charset = /(?:^|;)\s*charset\s*=\s*(?:"([^"]+)"|([^;\s]+))/i.exec(type);
  const label = charset?.[1] || charset?.[2] || "utf-8";
  const decoded = ["complete", "incomplete", "output_limit"].includes(payload.decoding);
  const partial = Boolean(body.truncated || body.complete === false || ["incomplete", "output_limit"].includes(payload.decoding));
  const compressed = encoding && encoding.toLowerCase() !== "identity" && body.observed_bytes > 0;
  const original = raw || Boolean(compressed && !decoded);
  const content = previewContent(payload.bytes, { pretty: !original, truncated: partial, forceHex: original, charset: label });
  let state = raw ? "Original entity bytes" : "";
  if (!raw && compressed) {
    state = ({
      complete: "decompressed",
      incomplete: "decoded prefix; compressed data is incomplete (checksum not verified)",
      output_limit: "decoded prefix limited to 1 MiB (checksum not verified)",
      invalid: "decompression failed; showing original bytes",
      unsupported: "encoding is not supported for preview; showing original bytes",
      identity: "compressed bytes; not decoded",
    })[payload.decoding] || "compressed bytes; not decoded";
    state = `Content-Encoding: ${encoding} · ${state}`;
  }
  const format = content.format === "hex" ? "Hex / ASCII" : content.json ? `JSON · ${label}` : label;
  const status = `${body.status} · retained ${body.captured_bytes} / observed ${body.observed_bytes} B${compressed ? " encoded" : ""}${body.limit_reason ? ` · ${body.limit_reason}` : ""} · ${format}${state ? ` · ${state}` : ""}${decoded ? ` · decoded ${payload.bytes.length} B` : ""}${content.incompleteUtf8 ? " · character cut at preview limit" : ""}`;
  return { ...content, status, partial };
}
