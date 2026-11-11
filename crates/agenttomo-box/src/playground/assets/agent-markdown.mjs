import { marked } from "./vendor/marked.mjs";
import DOMPurify from "./vendor/dompurify.mjs";

// Model output is untrusted. Render passive Markdown only, without remote media.
export function markdownContent(source) {
  const content = DOMPurify.sanitize(marked.parse(source, {async:false, gfm:true}), {
    ALLOWED_TAGS: ["p","br","strong","em","del","code","pre","blockquote","ul","ol","li","h1","h2","h3","h4","h5","h6","hr","table","thead","tbody","tr","th","td","a"],
    ALLOWED_ATTR: ["href","title","start","align"],
    ALLOW_DATA_ATTR: false,
    ALLOW_ARIA_ATTR: false,
    RETURN_DOM_FRAGMENT: true,
  });
  for (const link of content.querySelectorAll("a[href]")) {
    link.target = "_blank";
    link.rel = "noopener noreferrer";
  }
  return content;
}
