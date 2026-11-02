# -*- coding: utf-8 -*-
"""Generate PROPOSAL.md and fill.html from draft.py."""
import html, json
from draft import FIELDS, LIMITS, CHOICES

# (label, form field name, kind, limit, draft key, hint)
FORM = [
    ("Fund",                      "call", "select", None, None, "Pick this from the first dropdown"),
    ("Proposal title",            "project", "text", 100, "title", None),
    ("Project website / repository", "website", "text", 100, "repositories", "Website 2, 3 and 4 can stay empty"),
    ("Project summary",           "abstract", "textarea", 1000, "summary", "Shown as the summary field, max 1000 characters"),
    ("Amount requested (euros)",  "amount", "text", None, "amount", "Just the number, no currency symbol"),
    ("Proposed effort",           "use", "textarea", 4000, "effort", "Tasks, time, rate, expenses"),
    ("Comparison with other efforts", "comparison", "textarea", 4000, "comparison", "What is new and different"),
    ("Technical challenges",      "challenges", "textarea", 4000, "challenges", "What you expect to be hard"),
    ("Ecosystem",                 "ecosystem", "textarea", 2000, "ecosystem", "Who the users are and how you engage them"),
    ("Background",                "experience", "textarea", 2000, "background", "Why you are the right person"),
    ("Other funding",             "otherfunding", "textarea", 1000, "other_funding", None),
    ("AI disclosure: did you use AI in writing?", "used_ai", "select", None, None, "Choose the yes option if you paste this text unchanged"),
    ("AI disclosure: prompts and detail", "ai_prompt", "textarea", 8000, "ai_disclosure", "What AI was used for"),
    ("AI disclosure: does the codebase contain AI-generated code?", "codebase_used_ai", "select", None, None, None),
    ("AI disclosure: describe the AI use in the codebase", "codebase_ai_clarification", "textarea", 1000, "codebase_ai_clarification", None),
    ("Your name",                 "requestor", "text", 100, None, "You must fill this in"),
    ("Email address",             "email", "text", 100, None, "You must fill this in"),
    ("Phone number",              "phone", "text", 100, None, "Include the country code"),
    ("Type of applicant",         "entity", "select", None, None, None),
    ("Name of organisation",      "organization", "text", 100, None, "Arvion Technologies, if you apply as the company"),
    ("Country",                   "country", "select", None, None, "Nepal"),
]

def value_for(key):
    return FIELDS[key]

rows = []
for label, name, kind, limit, key, hint in FORM:
    if kind == "select":
        if name in CHOICES:
            val, human = CHOICES[name]
            rows.append({"label": label, "name": name, "kind": "select", "value": val,
                         "human": human, "limit": None, "hint": hint})
        else:
            rows.append({"label": label, "name": name, "kind": "select", "value": "",
                         "human": "You fill this in", "limit": None, "hint": hint})
    else:
        v = value_for(key) if key else ""
        rows.append({"label": label, "name": name, "kind": kind, "value": v,
                     "limit": limit, "hint": hint})

data = json.dumps(rows, ensure_ascii=False)

HTML = """<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>MeshDoc NLnet submission helper</title>
<style>
 :root { --bg:#0d1117; --panel:#161b22; --line:#30363d; --text:#e6edf3; --dim:#8b949e;
         --green:#3fb950; --blue:#58a6ff; --amber:#d29922; }
 * { box-sizing: border-box; }
 body { margin:0; background:var(--bg); color:var(--text);
        font:15px/1.55 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif; }
 header { position:sticky; top:0; background:#0d1117ee; backdrop-filter:blur(6px);
          border-bottom:1px solid var(--line); padding:14px 20px; z-index:10; }
 h1 { font-size:17px; margin:0 0 4px; }
 .sub { color:var(--dim); font-size:13px; }
 .bar { height:6px; background:#21262d; border-radius:3px; margin-top:10px; overflow:hidden; }
 .bar > i { display:block; height:100%; width:0; background:var(--green); transition:width .2s; }
 main { max-width:960px; margin:0 auto; padding:20px; }
 .card { background:var(--panel); border:1px solid var(--line); border-radius:10px;
         padding:14px 16px; margin:0 0 14px; }
 .card.done { border-color:var(--green); }
 .top { display:flex; align-items:baseline; gap:10px; flex-wrap:wrap; }
 .label { font-weight:600; }
 .field { color:var(--dim); font-size:12px; font-family:ui-monospace,monospace; }
 .hint { color:var(--amber); font-size:12px; margin-top:2px; }
 .target { font-family:ui-monospace,monospace; font-size:13px; color:var(--blue);
           word-break:break-all; margin:8px 0 4px; }
 textarea, .fakearea { width:100%; background:#0b0f14; color:var(--text);
          border:1px solid var(--line); border-radius:8px; padding:10px;
          font:13px/1.5 ui-monospace,monospace; resize:vertical; margin-top:8px; }
 textarea { min-height:90px; }
 .fakearea { white-space:pre-wrap; }
 .row { display:flex; align-items:center; gap:10px; margin-top:8px; flex-wrap:wrap; }
 button { background:#21262d; color:var(--text); border:1px solid var(--line);
          border-radius:8px; padding:7px 14px; font-size:13px; cursor:pointer; }
 button:hover { background:#30363d; }
 button.primary { background:#238636; border-color:#2ea043; }
 button.copied { background:var(--green); border-color:var(--green); color:#04120a; }
 .count { color:var(--dim); font-size:12px; font-family:ui-monospace,monospace; }
 .over { color:#f85149; font-weight:600; }
 .note { background:#1c2128; border-left:3px solid var(--amber); padding:12px 14px;
         border-radius:6px; margin:0 0 18px; font-size:14px; }
 .note b { color:var(--amber); }
 code { font-family:ui-monospace,monospace; background:#21262d; padding:1px 5px; border-radius:4px; }
</style></head>
<body>
<header>
  <h1>MeshDoc, NLnet Restack submission helper</h1>
  <div class="sub" id="status">0 of 0 fields copied</div>
  <div class="bar"><i id="progress"></i></div>
</header>
<main>
  <div class="note">
    <b>Before you paste.</b> Work top to bottom; each card matches one field on
    <code>nlnet.nl/propose/</code> and names the field it goes into. The counters show
    exactly what the form will count.
    <br><br>
    If you paste this text as it is, the AI disclosure answers below are already the
    truthful ones. If you rewrite the text in your own words, change
    <code>used_ai</code> to the "I did not use" option and edit the disclosure
    paragraph to say AI was used for research only. Do not claim the writing is
    entirely yours while it is not.
    <br><br>
    Deadline: 3 November, 16:45 Nepal time. You can submit and resubmit until then;
    only the last complete version counts.
  </div>
  <div id="app"></div>
</main>
<script>
const FIELDS = __DATA__;

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

let total = 0, done = 0;
const statusEl = document.getElementById('status');
const progressEl = document.getElementById('progress');

function refresh() {
  statusEl.textContent = done + ' of ' + total + ' fields copied';
  progressEl.style.width = total ? (done / total * 100) + '%' : '0%';
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (e) {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try { ok = document.execCommand('copy'); } catch (_) {}
    document.body.removeChild(ta);
    return ok;
  }
}

const app = document.getElementById('app');

FIELDS.forEach((f, i) => {
  if (!f.value && f.kind !== 'select') {
    // Personal fields the applicant fills in themselves.
    const card = el('div', 'card');
    const top = el('div', 'top');
    top.appendChild(el('span', 'label', f.label));
    top.appendChild(el('span', 'field', f.name));
    card.appendChild(top);
    if (f.hint) card.appendChild(el('div', 'hint', f.hint));
    const input = el('input');
    input.className = 'fakearea';
    input.style.minHeight = '0';
    input.placeholder = 'You fill this in';
    card.appendChild(input);
    app.appendChild(card);
    return;
  }

  total++;
  const card = el('div', 'card');
  card.id = 'card-' + i;

  const top = el('div', 'top');
  top.appendChild(el('span', 'label', f.label));
  top.appendChild(el('span', 'field', f.name + (f.limit ? ' (max ' + f.limit + ')' : '')));
  card.appendChild(top);
  if (f.hint) card.appendChild(el('div', 'hint', f.hint));

  const target = f.kind === 'select' ? f.value : f.value;
  if (f.kind === 'select') {
    const t = el('div', 'target', target);
    card.appendChild(t);
    card.appendChild(el('div', 'hint', 'Choose this option: ' + f.human));
  } else {
    const ta = el('textarea');
    ta.readOnly = true;
    ta.value = f.value;
    ta.rows = Math.min(14, Math.max(3, Math.ceil(f.value.length / 95)));
    card.appendChild(ta);
  }

  const row = el('div', 'row');
  const btn = el('button', 'primary', 'Copy');
  const mark = el('button', '', 'Mark done');
  const count = el('span', 'count');
  const len = f.kind === 'select' ? 0 : f.value.length;
  if (f.limit) {
    const left = f.limit - len;
    count.textContent = len + ' / ' + f.limit + ' (' + left + ' left)';
    if (left < 0) count.className = 'count over';
  } else {
    count.textContent = len + ' chars';
  }

  btn.onclick = async () => {
    const ok = await copy(f.kind === 'select' ? f.value : f.value);
    if (ok) {
      btn.textContent = 'Copied';
      btn.classList.add('copied');
      setTimeout(() => { btn.textContent = 'Copy'; btn.classList.remove('copied'); }, 1200);
    } else {
      btn.textContent = 'Select manually';
      const ta = card.querySelector('textarea');
      if (ta) { ta.focus(); ta.select(); }
    }
  };

  mark.onclick = () => {
    card.classList.toggle('done');
    done += card.classList.contains('done') ? 1 : -1;
    mark.textContent = card.classList.contains('done') ? 'Done' : 'Mark done';
    refresh();
  };

  row.appendChild(btn);
  row.appendChild(mark);
  row.appendChild(count);
  card.appendChild(row);
  app.appendChild(card);
});

refresh();
</script>
</body></html>
"""

open("fill.html", "w").write(HTML.replace("__DATA__", data))

# PROPOSAL.md, in the same order
out = ["# NLnet proposal, in form order", "",
       "Generated from draft.py. Every field is under the form's own maxlength and contains",
       "no em dash, en dash or horizontal bar. Open `fill.html` in a browser for one click",
       "copy per field, or copy from here.", "",
       "| # | Form field | Limit | Characters |", "|---|---|---|---|"]
n = 0
for f in rows:
    if not f["value"]:
        continue
    n += 1
    lim = f["limit"] if f["limit"] else "none"
    ln = 0 if f["kind"] == "select" else len(f["value"])
    out.append(f"| {n} | {f['label']} (`{f['name']}`) | {lim} | {ln} |")
out += ["", "---", ""]
n = 0
for f in rows:
    if not f["value"]:
        out.append(f"## {f['label']}")
        out.append("")
        out.append(f"Field `{f['name']}`: {f['hint'] or 'you fill this in'}.")
        out.append("")
        continue
    n += 1
    out.append(f"## {n}. {f['label']}")
    out.append("")
    out.append(f"Goes into the form field `{f['name']}`.")
    out.append("")
    if f["kind"] == "select":
        out.append(f"```text\n{f['value']}\n```")
        out.append("")
        out.append(f"That is the option: {f['human']}.")
    else:
        out.append("```text")
        out.append(f["value"])
        out.append("```")
        ln = len(f["value"])
        out.append("")
        out.append(f"{ln} characters" + (f" of {f['limit']}" if f["limit"] else "") + ".")
    out.append("")
open("PROPOSAL.md", "w").write("\n".join(out))

print("wrote PROPOSAL.md and fill.html")
print("fields:", n, "plus personal")
for f in rows:
    if f["limit"] and f["kind"] != "select" and len(f["value"]) > f["limit"]:
        print("OVER LIMIT:", f["name"])
