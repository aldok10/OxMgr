import { LOG } from "../core/const.js";

// ───────────────────────────────────────────────────────────────────────────
// Log line interpretation. Every function here is pure and runs ONCE per line
// at admission time; results are cached on the line record so re-rendering a
// row that scrolls back into view costs no reparsing.
// ───────────────────────────────────────────────────────────────────────────

// CSI sequences. We consume every one we match so no escape text can leak
// into the output, but only translate SGR (ending in "m"); the rest (cursor
// moves, erase) have no meaning in a scrollback view and are dropped.
const CSI_RE = /\u001b\[([0-9;:?]*)([A-Za-z])/g;
// OSC (window title etc.) and single-char escapes, also dropped.
const OSC_RE = /\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)?/g;
const LONE_ESC_RE = /\u001b[@-Z\\-_]?/g;

const ANSI_STYLE = { 1: "bold", 2: "dim", 3: "italic", 4: "underline" };
const ANSI_STYLE_OFF = { 21: "bold", 22: "bold", 23: "italic", 24: "underline" };

// Fold an SGR parameter list into a style state. Mutates and returns state so
// a long line does not allocate a new object per escape sequence.
const applySgr = (state, params) => {
  const codes = params.split(/[;:]/);
  for (let idx = 0; idx < codes.length; idx++) {
    const code = parseInt(codes[idx], 10);
    if (Number.isNaN(code) || code === 0) {
      state.fg = null; state.bg = null;
      state.bold = state.dim = state.italic = state.underline = false;
      continue;
    }
    if (ANSI_STYLE[code]) { state[ANSI_STYLE[code]] = true; continue; }
    if (code === 22) { state.bold = false; state.dim = false; continue; }
    if (ANSI_STYLE_OFF[code]) { state[ANSI_STYLE_OFF[code]] = false; continue; }
    if (code >= 30 && code <= 37) { state.fg = code - 30; continue; }
    if (code >= 90 && code <= 97) { state.fg = code - 90 + 8; continue; }
    if (code >= 40 && code <= 47) { state.bg = code - 40; continue; }
    if (code >= 100 && code <= 107) { state.bg = code - 100 + 8; continue; }
    if (code === 39) { state.fg = null; continue; }
    if (code === 49) { state.bg = null; continue; }
    // 256-colour / truecolour: "38;5;n" or "38;2;r;g;b". Map the 256-colour
    // form onto our 16-colour palette; skip truecolour's three operands.
    if (code === 38 || code === 48) {
      const mode = parseInt(codes[idx + 1], 10);
      const target = code === 38 ? "fg" : "bg";
      if (mode === 5) {
        const n = parseInt(codes[idx + 2], 10);
        state[target] = Number.isNaN(n) ? null : (n < 16 ? n : (n >= 232 ? 7 : ((n - 16) % 6 === 0 ? 4 : n % 8)));
        idx += 2;
      } else if (mode === 2) {
        state[target] = null;
        idx += 4;
      }
    }
  }
  return state;
};

const styleClasses = (state) => {
  const cls = [];
  if (state.fg !== null) cls.push(`a-fg-${state.fg}`);
  if (state.bg !== null && state.bg < 8) cls.push(`a-bg-${state.bg}`);
  if (state.bold) cls.push("a-bold");
  if (state.dim) cls.push("a-dim");
  if (state.italic) cls.push("a-italic");
  if (state.underline) cls.push("a-underline");
  return cls.length ? cls.join(" ") : "";
};

// Split a line into [{ text, cls }] segments. Style state is local to the
// line, so an unterminated sequence cannot bleed into the next line.
const parseAnsi = (raw) => {
  const state = { fg: null, bg: null, bold: false, dim: false, italic: false, underline: false };
  const segments = [];
  let cursor = 0, match;
  CSI_RE.lastIndex = 0;
  while ((match = CSI_RE.exec(raw)) !== null) {
    if (match.index > cursor) segments.push({ text: raw.slice(cursor, match.index), cls: styleClasses(state) });
    if (match[2] === "m") applySgr(state, match[1]);
    cursor = match.index + match[0].length;
  }
  if (cursor < raw.length) segments.push({ text: raw.slice(cursor), cls: styleClasses(state) });
  return segments;
};

// Strip every escape sequence without styling: used for plain lines and for
// deriving severity, where escapes would break token matching.
const stripAnsi = (raw) => raw.replace(CSI_RE, "").replace(OSC_RE, "").replace(LONE_ESC_RE, "");

const hasEscape = (raw) => raw.includes("\u001b");

const LEVELS = ["fatal", "error", "warn", "warning", "info", "debug", "trace"];
const LEVEL_ALIAS = { warning: "warn", err: "error", crit: "fatal", critical: "fatal", fatal: "fatal" };
// A level must appear as its own token, optionally bracketed, near the start of
// the line. Substring matching would call "/api/errors" an error line.
export const LEVEL_RE = new RegExp(`(?:^|[\\s\\[(])(${LEVELS.join("|")}|err|crit|critical)(?:[\\s\\]):]|$)`, "i");

const normalizeLevel = (word) => {
  const lower = word.toLowerCase();
  return LEVEL_ALIAS[lower] ?? lower;
};

// Severity from a bare log line: only the head is examined, because a level
// token belongs in the prefix and message bodies quote levels all the time.
const levelFromText = (text) => {
  const head = text.length > 120 ? text.slice(0, 120) : text;
  const match = head.match(LEVEL_RE);
  return match ? normalizeLevel(match[1]) : null;
};

const LEVEL_FIELDS = ["level", "lvl", "severity", "levelname", "log.level"];
const levelFromJson = (obj) => {
  for (const field of LEVEL_FIELDS) {
    const val = obj[field];
    if (typeof val === "string" && val) {
      const norm = normalizeLevel(val.trim());
      if (LEVELS.includes(norm) || LEVEL_ALIAS[norm]) return normalizeLevel(norm);
    }
    // Some loggers emit numeric levels (bunyan/pino): map the common bands.
    if (typeof val === "number") {
      if (val >= 60) return "fatal";
      if (val >= 50) return "error";
      if (val >= 40) return "warn";
      if (val >= 30) return "info";
      if (val >= 20) return "debug";
      return "trace";
    }
  }
  return null;
};

// Cheap gate before JSON.parse: running parse-in-try on every line of a
// plain-text stream would dominate per-line cost and allocate on each failure.
const looksJson = (text) => {
  if (text.length < 2 || text.length > 200000) return false;
  const first = text.charCodeAt(0);
  return (first === 123 /* { */ || first === 91 /* [ */) && /[}\]]$/.test(text);
};

const parseJsonLine = (text) => {
  if (!looksJson(text)) return null;
  try {
    const val = JSON.parse(text);
    return (val && typeof val === "object") ? val : null;
  } catch { return null; }
};

// ISO-8601-ish or bracketed clock timestamps at the head of a line.
const TS_RE = /^(\[?\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d{1,9})?(?:Z|[+-]\d{2}:?\d{2})?\]?|\[?\d{2}:\d{2}:\d{2}(?:[.,]\d{1,9})?\]?)/;

// Build the immutable record a line is stored as. Everything expensive is done
// here, once. `text` is the display form (escapes removed, truncated); `segs`
// is the styled form when the line carried ANSI; `json` is set when the line
// is a whole JSON value.
export const makeLine = (raw, stream, seq) => {
  const input = typeof raw === "string" ? raw : String(raw ?? "");
  const styled = hasEscape(input);
  const plainFull = styled ? stripAnsi(input) : input;
  const truncated = plainFull.length > LOG.MAX_LINE_CHARS;
  const plain = truncated ? plainFull.slice(0, LOG.MAX_LINE_CHARS) : plainFull;

  // The file-tail path prefixes each line with the daemon's own timestamp
  // ("<ts>: <line>", from log_date_format), while the event-bus path delivers
  // the raw line. Detection must look past that prefix or the same JSON line
  // renders coloured in one view and plain in the other.
  const tsMatch = plain.match(TS_RE);
  let preLen = tsMatch ? tsMatch[0].length : 0;
  if (preLen) {
    const sep = plain.slice(preLen).match(/^:?[ \t]+/);
    if (sep) preLen += sep[0].length;
  }
  const body = preLen ? plain.slice(preLen) : plain;

  const json = truncated ? null : parseJsonLine(body);
  const level = json ? levelFromJson(json) : levelFromText(plain);

  return {
    // Position of this line in whatever it came from: the arrival count for a live
    // stream, the file line number for a paged file. Assigned by the view at
    // admission, because only it knows which of those applies. Rendering the gutter
    // from this rather than from `dropped + bufferIndex + 1` is what keeps a number
    // stable across eviction, prepending and filtering — the old expression assumed
    // the buffer always held the tail of the stream, which paging makes false.
    seq,
    stream,
    text: plain,
    // Styled segments are only retained when the line actually had escapes,
    // and are derived from the (possibly truncated) input to bound retention.
    segs: styled && !json ? parseAnsi(truncated ? input.slice(0, LOG.MAX_LINE_CHARS * 2) : input) : null,
    json,
    level,
    // Length of the leading timestamp, dimmed on render. For a JSON line this
    // also marks how much of the text precedes the object, so the prefix is
    // still shown rather than swallowed by the pretty-printer.
    tsLen: tsMatch ? tsMatch[0].length : 0,
    preLen: json ? preLen : 0,
    truncated,
    bytes: plain.length,
  };
};
