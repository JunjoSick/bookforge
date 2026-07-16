const CSRF_HEADER = "x-bookforge-csrf";
const CSRF_TOKEN = "__BOOKFORGE_CSRF_TOKEN__";
const $ = (sel, el) => (el || document).querySelector(sel);
const ESC = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
function esc(value) { return String(value == null ? "" : value).replace(/[&<>"']/g, ch => ESC[ch]); }
function num(n) { return (n || 0).toLocaleString(); }
function pct(done, total) { return total > 0 ? Math.min(100, Math.round(done / total * 100)) : 0; }
function shorten(s, n) { s = s || ""; return s.length > n ? s.slice(0, n - 1) + "…" : s; }
function badgeClass(status) { return (status || "").toLowerCase().replace(/[^a-z]/g, ""); }
function titleFromPath(path) {
  let base = String(path || "").split(/[\\/]/).pop() || "book";
  base = base.replace(/\.epub$/i, "").replace(/^\d{6,}-/, "").replace(/[._-]+/g, " ").trim();
  return base ? base.charAt(0).toUpperCase() + base.slice(1) : "Untitled";
}
function fmtDur(secs) {
  secs = Math.round(secs);
  if (secs <= 0) return "—";
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
  return h > 0 ? `${h}h${String(m).padStart(2,"0")}m` : (m > 0 ? `${m}m${String(s).padStart(2,"0")}s` : `${s}s`);
}
function elapsedSecs(s) {
  if (s.finished && s.finished_elapsed_ms != null) return s.finished_elapsed_ms / 1000;
  if (s.first_timestamp_ms != null && s.last_timestamp_ms != null && s.last_timestamp_ms >= s.first_timestamp_ms)
    return (s.last_timestamp_ms - s.first_timestamp_ms) / 1000;
  return 0;
}
function segPerMin(s) { const e = elapsedSecs(s); return e > 0 ? (s.done_segments || 0) / e * 60 : 0; }
function etaSecs(s) { const r = Math.max(0, (s.total_segments || 0) - (s.done_segments || 0)); const pm = segPerMin(s); return pm > 0 ? r / (pm / 60) : 0; }
function fmtCost(v) { return v == null ? "n/a" : "$" + Number(v).toFixed(2); }

const QUALITY = [
  { id:"economy",  name:"Economy",  desc:"Fast, very cheap. Good for drafts.", profile:"fastest",  provider:"deepseek", model:"deepseek-v4-flash" },
  { id:"balanced", name:"Balanced", desc:"Strong quality, sensible price.",   profile:"balanced", provider:"deepseek", model:"deepseek-v4-flash", rec:true },
  { id:"finest",   name:"Finest",   desc:"Top models, literary register.",    profile:"safe",     provider:"openrouter", model:"openrouter/auto" },
];

const App = {
  screen: "library",
  theme: localStorage.getItem("bf-theme") || "light",
  jobs: [],
  selected: null,
  es: null,
  options: { languages: ["English","Italian","Spanish","French","German"], providers: [], audio_providers: [], ffmpeg_available: false },
  providerKeys: {},
  wizard: null,
  audioWizard: null,
  audioSelected: null,
  runtimeSettings: null,
  runtimeJob: null,
  runtimeRefreshPending: false,
};

function freshWizard() {
  return { step:0, file:null, fileName:"", from:"", to:"Italian", quality:"balanced",
    provider:"deepseek", model:"deepseek-v4-flash", profile:"balanced",
    advancedOpen:false, concurrency:4, qa:"suspicious", context:3, validate:false,
    apiKey:"", baseUrl:"", estimate:null, status:"" };
}

function freshAudioWizard() {
  const canM4b = App.options.ffmpeg_available === true;
  return { file:null, fileName:"", provider:"openai", model:"gpt-4o-mini-tts",
    voice:"alloy", format:"mp3", speed:1, maxChars:2000, concurrency:4,
    instructions:"", baseUrl:"", apiKey:"", stitch:canM4b, m4b:canM4b,
    launching:false, status:"" };
}

function applyTheme() {
  document.documentElement.setAttribute("data-theme", App.theme);
  $("#sun").classList.toggle("on", App.theme === "light");
  $("#moon").classList.toggle("on", App.theme === "dark");
}
function bfTheme() { App.theme = App.theme === "dark" ? "light" : "dark"; localStorage.setItem("bf-theme", App.theme); applyTheme(); }

function bfGo(screen, opts) { Object.assign(App, opts || {}); App.screen = screen; if (screen !== "progress") closeStream(); render(); }
function bfStartNew() { App.wizard = freshWizard(); App.screen = "wizard"; closeStream(); render(); }
function bfStartAudiobook() { App.audioWizard = freshAudioWizard(); App.audioSelected = null; localStorage.removeItem("bf-audiobook-id"); App.screen = "audiobook"; closeStream(); render(); }

const NAV = [["library","Library"],["audiobook","Audiobooks"],["progress","Progress"],["review","Review"],["validation","Validation"],["glossary","Glossary"]];
function renderNav() {
  const active = App.screen === "wizard" ? "library" : App.screen;
  $("#nav").innerHTML = NAV.map(([id,label]) =>
    `<div class="tab ${id===active?"active":""}" onclick="bfGo('${id}')">${label}</div>`).join("");
}

function render() {
  renderNav();
  const stage = $("#stage");
  switch (App.screen) {
    case "library": return renderLibrary(stage);
    case "wizard": return renderWizard(stage);
    case "audiobook": return renderAudiobook(stage);
    case "progress": return renderProgress(stage);
    case "review": return renderReview(stage);
    case "validation": return renderValidation(stage);
    case "glossary": return renderGlossary(stage);
    default: return renderLibrary(stage);
  }
}

/* ---------------- Library ---------------- */
function jobDone(st) { return st === "succeeded" || st === "done" || st === "completed"; }
async function renderLibrary(stage) {
  stage.innerHTML = `<div class="wrap">
    <div class="pagehead"><div><h1>Your library</h1><p>Pick up a translation, review a finished book, or start a new one.</p></div>
      <button class="btn btn-primary" onclick="bfStartNew()">+ New translation</button></div>
    <div class="book-grid" id="grid"><div class="empty">Loading…</div></div></div>`;
  loadLibraryJobs();
}
async function loadLibraryJobs() {
  let jobs = [];
  try { jobs = await (await fetch("/api/jobs")).json(); } catch (e) { jobs = []; }
  App.jobs = jobs;
  const grid = $("#grid");
  if (!grid || App.screen !== "library") return;
  const cards = jobs.map(j => {
    const p = pct(j.done, j.total_segments);
    const st = badgeClass(j.status);
    const done = jobDone(st);
    const action = done ? "Review →" : (st === "failed" || st === "error") ? "Inspect →" : (p > 0 ? "View progress →" : "Open →");
    const title = titleFromPath(j.input_path);
    return `<div class="book-card" onclick="bfOpenJob('${esc(j.id)}','${st}')">
      <div class="cover">${esc(title.charAt(0))}</div>
      <div class="book-main">
        <div class="book-title">${esc(title)}</div>
        <div class="book-sub">${esc(j.provider)} / ${esc(j.model)}</div>
        <div class="book-meta"><span class="badge ${st}">${esc(j.status)}</span><span class="mono">${j.done}/${j.total_segments} · ${esc(j.target_lang)}</span></div>
        <div class="bar-track" ${p?"":'style="opacity:0"'}><div class="bar-fill" style="width:${p}%;${done?"background:var(--good)":""}"></div></div>
      </div>
      <div class="book-action">${action}</div></div>`;
  }).join("");
  grid.innerHTML = cards + `<div class="add-card" onclick="bfStartNew()">
      <div class="plus">＋</div><b>Translate a new book</b><span>Drop an EPUB to begin</span></div>`;
}
function bfOpenJob(id, st) {
  if (jobDone(st)) bfGo("review", { selected: id });
  else bfGo("progress", { selected: id });
}

/* ---------------- Wizard ---------------- */
const WIZ_STEPS = [
  { label:"Book", hint:"Source file" },
  { label:"Languages", hint:"Pair" },
  { label:"Quality", hint:"Tier" },
  { label:"Review & start", hint:"Confirm plan" },
];
const WIZ_META = [
  ["Step 1 · Your book","Pick the source file","This is the EPUB BookForge will translate. Structure, footnotes and code blocks are protected."],
  ["Step 2 · Languages","Choose the pair","Pick the source (or leave it to auto-detect) and the target language."],
  ["Step 3 · Quality","How good, how cheap","Sets the model and pricing tier. Fine-tune the exact model under Advanced on the next step."],
  ["Step 4 of 4 · Review","Ready when you are","Review the plan, then start. The job is checkpointed every chapter, so you can resume or retry anytime."],
];
function providerOption(id) { return (App.options.providers || []).find(p => p.id === id) || (App.options.providers || [])[0] || { id:"mock", models:[], requires_key:false, requires_base_url:false }; }

function renderWizard(stage) {
  const w = App.wizard || (App.wizard = freshWizard());
  const meta = WIZ_META[w.step];
  const rail = WIZ_STEPS.map((st,i) => {
    const cls = i < w.step ? "done" : i === w.step ? "current" : "";
    return `<div class="step ${cls}" onclick="bfWizGo(${i})"><span class="dot">${i<w.step?"✓":i+1}</span>
      <div style="flex:1"><div class="lbl">${st.label}</div><div class="hint">${st.hint}</div></div></div>`;
  }).join("");
  stage.innerHTML = `<div class="wiz">
    <div class="rail"><div class="kicker">New translation</div><div class="steps">${rail}</div>
      <div class="wizsummary"><div class="kicker">Translating</div>
        <div class="t">${esc(w.fileName ? titleFromPath(w.fileName) : "No file yet")}</div>
        <div class="m">${esc((w.from||"auto"))} → ${esc(w.to||"?")} · ${esc(qualityName(w.quality))}</div></div></div>
    <div class="wizpanel"><div class="kicker">${meta[0]}</div><h2>${meta[1]}</h2><div class="sub">${meta[2]}</div>
      <div class="wizbody" id="wizbody"></div>
      <div class="wizfoot">
        <button class="btn btn-ghost" onclick="bfWizBack()" ${w.step===0?"hidden":""}>Back</button>
        <span class="grow"></span><span class="launchstatus" id="launchstatus">${esc(w.status||"")}</span>
        <button class="btn btn-primary" id="wiznext" style="padding:13px 26px;font-size:14px" onclick="bfWizNext()">${w.step===3?"Start translation":"Continue"}</button>
      </div></div></div>`;
  renderWizBody();
}
function qualityName(id) { const q = QUALITY.find(q => q.id === id); return q ? q.name : id; }
function bfWizGo(i) { syncWizInputs(); App.wizard.step = Math.max(0, Math.min(3, i)); renderWizard($("#stage")); }
function bfWizBack() { syncWizInputs(); if (App.wizard.step > 0) { App.wizard.step--; renderWizard($("#stage")); } }

function renderWizBody() {
  const w = App.wizard, body = $("#wizbody"); if (!body) return;
  if (w.step === 0) {
    body.innerHTML = `<div class="drop ${w.file?"has":""}" onclick="$('#fileinput').click()"
        ondragover="bfDragOver(event)" ondragenter="bfDragOver(event)" ondragleave="bfDragLeave(event)" ondrop="bfDropFile(event)">
      ${w.file ? `<div class="fname">${esc(w.fileName)}</div><div style="color:var(--muted);font-size:12px;margin-top:6px">Click to choose a different file</div>`
               : `<div>Drop an <b>EPUB</b> here or click to browse.</div>`}
      </div><input type="file" id="fileinput" accept=".epub" hidden onchange="bfPickFile(this)">`;
  } else if (w.step === 1) {
    const chips = ["Italian","Spanish","French","German","Japanese","Korean","Portuguese","Chinese (Simplified)"];
    body.innerHTML = `<div class="lang-row">
        <div class="col"><div class="field-label">Translate from</div>
          <input class="inp" id="w_from" list="langs" placeholder="Auto-detect" value="${esc(w.from)}"></div>
        <div class="swap" onclick="bfSwapLangs()">⇄</div>
        <div class="col"><div class="field-label">Into</div>
          <input class="inp" id="w_to" list="langs" placeholder="Type a language…" value="${esc(w.to)}"></div>
      </div>
      <datalist id="langs">${(App.options.languages||[]).map(l=>`<option value="${esc(l)}">`).join("")}</datalist>
      <div class="field-label">Quick pick</div>
      <div class="chips">${chips.map(n=>`<div class="chip ${w.to===n?"on":""}" onclick="bfPickTo('${esc(n)}')">${esc(n)}</div>`).join("")}</div>`;
  } else if (w.step === 2) {
    body.innerHTML = `<div class="tiers">${QUALITY.map(q=>`
      <div class="tier ${w.quality===q.id?"on":""} ${q.rec?"rec":""}" onclick="bfPickTier('${q.id}')">
        <div class="tbadge">${w.quality===q.id?"Selected":q.rec?"Recommended":""}</div>
        <div class="tn">${q.name}</div><div class="td">${q.desc}</div>
        <div class="tm">${esc(q.provider)} · ${esc(q.model)}</div></div>`).join("")}</div>
      <p style="font:400 12.5px var(--sans);color:var(--muted)">You can override the provider and exact model under <b>Advanced</b> on the next step — including the offline <b>mock</b> provider for a dry run.</p>`;
  } else {
    renderReviewStep(body);
  }
}
function syncWizInputs() {
  const w = App.wizard; if (!w) return;
  const from = $("#w_from"); if (from) w.from = from.value.trim();
  const to = $("#w_to"); if (to) w.to = to.value.trim();
  if (w.to.toLowerCase() === "toki pona" && w.qa === "suspicious") w.qa = "off";
  const key = $("#w_key"); if (key) w.apiKey = key.value;
  const base = $("#w_base"); if (base) w.baseUrl = base.value.trim();
  const conc = $("#w_conc"); if (conc) w.concurrency = Math.max(1, Math.min(16, parseInt(conc.value,10) || 1));
  const mid = $("#w_modelid"); if (mid && mid.value.trim()) w.model = mid.value.trim();
}
function bfPickFile(input) {
  const f = input.files && input.files[0];
  if (!f) return;
  App.wizard.file = f; App.wizard.fileName = f.name; App.wizard.estimate = null;
  renderWizard($("#stage"));
}
/* Drag-and-drop onto a .drop zone. dragover/dragenter must preventDefault or the
   browser navigates to the dropped file instead of firing our drop handler. */
function bfDragOver(e) { e.preventDefault(); if (e.dataTransfer) e.dataTransfer.dropEffect = "copy"; e.currentTarget.classList.add("dragging"); }
function bfDragLeave(e) { e.currentTarget.classList.remove("dragging"); }
function bfDroppedEpub(e) {
  e.preventDefault();
  if (e.currentTarget) e.currentTarget.classList.remove("dragging");
  const f = e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files[0];
  if (!f) return null;
  if (!/\.epub$/i.test(f.name)) return null;
  return f;
}
function bfDropFile(e) {
  const f = bfDroppedEpub(e);
  if (!f) { toastWiz("drop an EPUB (.epub) file"); return; }
  App.wizard.file = f; App.wizard.fileName = f.name; App.wizard.estimate = null;
  renderWizard($("#stage"));
}
function bfSwapLangs() { syncWizInputs(); const w = App.wizard; const t = w.from; w.from = w.to; w.to = t; renderWizBody(); }
function bfPickTo(name) {
  syncWizInputs(); App.wizard.to = name;
  if (name.toLowerCase() === "toki pona" && App.wizard.qa === "suspicious") App.wizard.qa = "off";
  renderWizBody();
}
function bfPickTier(id) {
  const q = QUALITY.find(q => q.id === id); if (!q) return;
  const w = App.wizard; w.quality = id; w.profile = q.profile; w.provider = q.provider; w.model = q.model;
  w.estimate = null; renderWizBody();
}

function renderReviewStep(body) {
  const w = App.wizard;
  const opt = providerOption(w.provider);
  const needsKey = opt.requires_key === true && App.providerKeys[w.provider] !== true;
  const needsBase = opt.requires_base_url === true;
  const facts = [
    { k:"Languages", v:`${w.from||"auto"} → ${w.to||"?"}` },
    { k:"Quality", v:qualityName(w.quality) },
    { k:"Model", v:`${esc(w.provider)} · ${esc(w.model)}`, mono:true },
    { k:"Profile", v:esc(w.profile) },
  ];
  const est = w.estimate;
  const costLabel = est ? fmtCost(est.cost_usd) : (w.file ? "…" : "add a file");
  const tokens = est ? num(est.input_tokens + est.output_tokens) : "—";
  const providerChips = (App.options.providers||[]).map(p =>
    `<div class="chip ${w.provider===p.id?"on":""}" onclick="bfPickProvider('${p.id}')">${esc(p.label||p.id)}</div>`).join("");
  const models = (opt.models||[]).map(m =>
    `<div class="modelcard ${w.model===m?"on":""}" onclick="bfPickModel('${esc(m)}')"><div style="min-width:0"><div class="ml">${esc(m)}</div></div></div>`).join("");
  body.innerHTML = `
    <div class="facts">${facts.map(f=>`<div class="fact"><div class="k">${f.k}</div><div class="v ${f.mono?"mono":""}">${f.v}</div></div>`).join("")}</div>
    <div class="costbox"><div><div class="ck">Estimated cost</div><div class="cv" id="costv">${costLabel}</div></div>
      <div class="cm"><span id="esttokens">${tokens}</span> tokens<br>${est?"priced from catalog":"parses your EPUB"}</div></div>
    <div class="advtoggle" onclick="bfToggleAdvanced()"><span><span style="color:var(--accent)">⚙</span> Advanced — provider, model, concurrency, QA, context, validation</span><span>${w.advancedOpen?"▾":"▸"}</span></div>
    <div class="advbody" ${w.advancedOpen?"":"hidden"}>
      <div class="field-label">Provider</div>
      <div class="chips" style="margin-bottom:15px">${providerChips}</div>
      ${needsBase?`<div class="field-label">Base URL</div><input class="inp" id="w_base" placeholder="https://api.example.com/v1" value="${esc(w.baseUrl)}" style="margin-bottom:15px">`:""}
      ${needsKey?`<div class="field-label">API key</div><input class="inp" id="w_key" type="password" autocomplete="off" placeholder="Paste once for this session" value="${esc(w.apiKey)}"><div class="keyline">Read from the server environment or remembered for this server session.</div>`:""}
      <div class="field-label" style="margin-top:15px">Model · ${esc((opt.label||opt.id))}</div>
      <div class="modelcards">${models||`<div style="color:var(--faint);font-size:12px;padding:8px">No preset models</div>`}</div>
      <div style="display:flex;align-items:center;gap:8px;margin:2px 0 6px">
        <span style="font:400 11px var(--sans);color:var(--faint);white-space:nowrap">Or type any ID</span>
        <input class="inp" id="w_modelid" placeholder="provider/model-name" value="${esc(w.model)}" oninput="App.wizard.model=this.value.trim();App.wizard.estimate=null" onchange="requestEstimate()"></div>
      <div class="adv-grid" style="margin-top:6px">
        <div class="adv-cell"><span>Concurrency</span><div class="stepper">
          <div class="stepbtn" onclick="bfConc(-1)">−</div>
          <input class="numin" id="w_conc" type="number" min="1" max="16" value="${w.concurrency}">
          <div class="stepbtn" onclick="bfConc(1)">+</div></div></div>
        <div class="adv-cell" onclick="bfCycleQa()"><span>QA pass</span><span class="val">${esc(w.qa)}</span></div>
        <div class="adv-cell" onclick="bfCycleContext()"><span>Context window</span><span class="val">${w.context}</span></div>
        <div class="adv-cell" onclick="bfToggleValidate()"><span>Validate output</span><span class="val" style="${w.validate?"color:var(--good)":""}">${w.validate?"On":"Off"}</span></div>
      </div>
    </div>`;
  if (w.file && !w.estimate) requestEstimate();
}
function bfToggleAdvanced() { syncWizInputs(); App.wizard.advancedOpen = !App.wizard.advancedOpen; renderReviewStep($("#wizbody")); }
function bfPickProvider(id) { syncWizInputs(); const w = App.wizard; w.provider = id; const opt = providerOption(id); w.model = opt.default_model || (opt.models||[])[0] || w.model; w.estimate = null; renderReviewStep($("#wizbody")); }
function bfPickModel(m) { syncWizInputs(); App.wizard.model = m; App.wizard.estimate = null; renderReviewStep($("#wizbody")); }
function bfConc(d) { const el = $("#w_conc"); if (!el) return; let v = (parseInt(el.value,10)||1) + d; v = Math.max(1, Math.min(16, v)); el.value = v; App.wizard.concurrency = v; }
function bfCycleQa() { syncWizInputs(); const w = App.wizard; w.qa = w.qa === "off" ? "suspicious" : w.qa === "suspicious" ? "all" : "off"; renderReviewStep($("#wizbody")); }
function bfCycleContext() { syncWizInputs(); const w = App.wizard; w.context = w.context >= 6 ? 0 : w.context + 1; renderReviewStep($("#wizbody")); }
function bfToggleValidate() { syncWizInputs(); App.wizard.validate = !App.wizard.validate; renderReviewStep($("#wizbody")); }

async function requestEstimate() {
  const w = App.wizard; if (!w.file) return;
  const fd = new FormData();
  fd.append("file", w.file); fd.append("provider", w.provider);
  if (w.model) fd.append("model", w.model);
  if (w.to) fd.append("target", w.to);
  try {
    const r = await fetch("/api/estimate", { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN }, body: fd });
    const j = await r.json();
    if (!r.ok) return;
    if (App.screen === "wizard" && App.wizard === w) {
      w.estimate = j;
      const cv = $("#costv"); if (cv) cv.textContent = fmtCost(j.cost_usd);
      const et = $("#esttokens"); if (et) et.textContent = num(j.input_tokens + j.output_tokens);
    }
  } catch (e) {}
}

async function bfWizNext() {
  syncWizInputs();
  const w = App.wizard;
  if (w.step === 0) { if (!w.file) { toastWiz("choose an EPUB file"); return; } w.step = 1; return renderWizard($("#stage")); }
  if (w.step === 1) { if (!w.to) { toastWiz("choose a target language"); return; } w.step = 2; return renderWizard($("#stage")); }
  if (w.step === 2) { w.step = 3; return renderWizard($("#stage")); }
  return launchTranslation();
}
function toastWiz(msg) { const el = $("#launchstatus"); if (el) el.textContent = msg; if (App.wizard) App.wizard.status = msg; }

async function launchTranslation() {
  const w = App.wizard;
  const opt = providerOption(w.provider);
  if (opt.requires_base_url && !w.baseUrl) { w.advancedOpen = true; renderWizBody(); return toastWiz("base URL is required for this provider"); }
  if (w.launching) return;
  w.launching = true;
  const btn = $("#wiznext");
  const reenable = () => { w.launching = false; if (btn) { btn.disabled = false; btn.style.opacity = ""; btn.textContent = "Start translation"; } };
  if (btn) { btn.disabled = true; btn.style.opacity = ".6"; btn.textContent = "Starting…"; }
  toastWiz("uploading…");
  const fd = new FormData();
  fd.append("file", w.file);
  fd.append("target", w.to);
  if (w.from) fd.append("source", w.from);
  fd.append("provider", w.provider);
  if (w.model) fd.append("model", w.model);
  fd.append("profile", w.profile);
  fd.append("concurrency", String(w.concurrency));
  fd.append("qa", w.qa);
  fd.append("context_window", String(w.context));
  if (w.validate) fd.append("validate_output", "true");
  if (w.apiKey) fd.append("api_key", w.apiKey);
  if (w.baseUrl) fd.append("base_url", w.baseUrl);
  try {
    const r = await fetch("/api/translate", { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN }, body: fd });
    const j = await r.json();
    if (!r.ok) { reenable(); toastWiz(j.error || "launch failed"); return; }
    toastWiz("started — locating job…");
    await loadProviderStatus();
    trySelectPending(j.input_path, 0);
  } catch (e) { reenable(); toastWiz("launch failed"); }
}
async function trySelectPending(inputPath, attempt) {
  if (attempt > 25) return;
  let jobs = [];
  try { jobs = await (await fetch("/api/jobs")).json(); } catch (e) {}
  const match = jobs.find(j => j.input_path === inputPath);
  if (match) { bfGo("progress", { selected: match.id }); return; }
  setTimeout(() => trySelectPending(inputPath, attempt + 1), 900);
}

/* ---------------- Audiobooks ---------------- */
function audioProviderOption(id) {
  return (App.options.audio_providers || []).find(p => p.id === id)
    || { id:"mock", label:"mock", models:["mock-silence"], default_model:"mock-silence",
      default_voice:"mock", formats:["wav"], default_format:"wav", requires_key:false,
      requires_voice:false, supports_instructions:false, supports_speed:true };
}
function bfAudioProvider(id) {
  syncAudioInputs();
  const w = App.audioWizard, p = audioProviderOption(id);
  w.provider = id; w.model = p.default_model; w.voice = p.default_voice;
  w.format = p.default_format; w.speed = 1; w.instructions = ""; w.status = "";
  renderAudiobook($("#stage"));
}
function bfAudioFormat(format) { syncAudioInputs(); App.audioWizard.format = format; renderAudiobook($("#stage")); }
function bfAudioPickFile(input) {
  const file = input.files && input.files[0]; if (!file) return;
  App.audioWizard.file = file; App.audioWizard.fileName = file.name; renderAudiobook($("#stage"));
}
function bfAudioDropFile(e) {
  const f = bfDroppedEpub(e);
  if (!f) { audioToast("drop an EPUB (.epub) file"); return; }
  App.audioWizard.file = f; App.audioWizard.fileName = f.name; renderAudiobook($("#stage"));
}
function syncAudioInputs() {
  const w = App.audioWizard; if (!w) return;
  const model = $("#a_model"); if (model) w.model = model.value.trim();
  const voice = $("#a_voice"); if (voice) w.voice = voice.value.trim();
  const speed = $("#a_speed"); if (speed) w.speed = Number(speed.value) || 1;
  const maxChars = $("#a_chars"); if (maxChars) w.maxChars = Math.max(1, Math.min(4096, parseInt(maxChars.value,10)||2000));
  const concurrency = $("#a_conc"); if (concurrency) w.concurrency = Math.max(1, Math.min(16, parseInt(concurrency.value,10)||4));
  const instructions = $("#a_instructions"); if (instructions) w.instructions = instructions.value.trim();
  const baseUrl = $("#a_base"); if (baseUrl) w.baseUrl = baseUrl.value.trim();
  const key = $("#a_key"); if (key) w.apiKey = key.value;
  const stitch = $("#a_stitch"); if (stitch) w.stitch = stitch.checked;
  const m4b = $("#a_m4b"); if (m4b) w.m4b = m4b.checked;
}
function audioToast(message) { const el = $("#audio-status"); if (el) el.textContent = message; if (App.audioWizard) App.audioWizard.status = message; }

function renderAudiobook(stage) {
  if (App.audioSelected) return renderAudiobookProgress(stage, App.audioSelected);
  const w = App.audioWizard || (App.audioWizard = freshAudioWizard());
  const p = audioProviderOption(w.provider);
  const needsKey = p.requires_key && App.providerKeys[`audio:${w.provider}`] !== true;
  const providerChips = (App.options.audio_providers || []).map(provider =>
    `<div class="chip ${w.provider===provider.id?"on":""}" onclick="bfAudioProvider('${provider.id}')">${esc(provider.label)}</div>`).join("");
  const formatChips = (p.formats || []).map(format =>
    `<div class="chip ${w.format===format?"on":""}" onclick="bfAudioFormat('${format}')">${format.toUpperCase()}</div>`).join("");
  stage.innerHTML = `<div class="wrap">
    <div class="pagehead"><div><h1>Create an audiobook</h1><p>Narrate an original or translated EPUB directly. Translation is optional.</p></div>
      <button class="btn btn-ghost" onclick="bfGo('library')">Back to library</button></div>
    <div class="wizpanel" style="max-width:920px;margin:0 auto">
      <div class="kicker">Source EPUB</div>
      <div class="drop ${w.file?"has":""}" onclick="$('#audio-file').click()"
          ondragover="bfDragOver(event)" ondragenter="bfDragOver(event)" ondragleave="bfDragLeave(event)" ondrop="bfAudioDropFile(event)">
        ${w.file ? `<div class="fname">${esc(w.fileName)}</div><div style="color:var(--muted);font-size:12px;margin-top:6px">Click to choose another EPUB</div>` : `<div>Drop an <b>EPUB</b> here or click to browse.</div>`}
      </div><input type="file" id="audio-file" accept=".epub" hidden onchange="bfAudioPickFile(this)">
      <div class="field-label" style="margin-top:20px">Speech provider</div><div class="chips">${providerChips}</div>
      <div class="adv-grid" style="margin-top:18px">
        <div><div class="field-label">Model</div><input class="inp" id="a_model" value="${esc(w.model)}" list="audio-models"></div>
        <div><div class="field-label">${p.requires_voice?"Voice ID":"Voice"}</div><input class="inp" id="a_voice" value="${esc(w.voice)}" placeholder="${p.requires_voice?"Required ElevenLabs voice ID":"Voice name"}"></div>
        <div><div class="field-label">Speed</div><input class="inp" id="a_speed" type="number" min="0.25" max="4" step="0.05" value="${w.speed}" ${p.supports_speed?"":"disabled"}></div>
        <div><div class="field-label">Characters per request</div><input class="inp" id="a_chars" type="number" min="1" max="4096" value="${w.maxChars}"></div>
        <div><div class="field-label">Concurrency</div><input class="inp" id="a_conc" type="number" min="1" max="16" value="${w.concurrency}"></div>
        <div><div class="field-label">Format</div><div class="chips">${formatChips}</div></div>
      </div>
      <datalist id="audio-models">${(p.models||[]).map(model=>`<option value="${esc(model)}">`).join("")}</datalist>
      ${p.supports_instructions?`<div class="field-label" style="margin-top:18px">Narration instructions</div><textarea class="inp" id="a_instructions" rows="3" placeholder="Tone, pronunciation, or delivery guidance">${esc(w.instructions)}</textarea>`:""}
      ${w.provider==="openai"?`<div class="field-label" style="margin-top:18px">Optional OpenAI-compatible base URL</div><input class="inp" id="a_base" value="${esc(w.baseUrl)}" placeholder="https://api.openai.com/v1 or http://127.0.0.1:8880/v1">`:""}
      ${needsKey?`<div class="field-label" style="margin-top:18px">API key</div><input class="inp" id="a_key" type="password" autocomplete="off" placeholder="Held in memory for this dashboard session"><div class="keyline">The key is injected into the child process and is never put on the command line or disk.</div>`:""}
      <div class="facts" style="margin-top:20px">
        <label class="fact"><div class="k">Chapter files</div><div class="v"><input id="a_stitch" type="checkbox" ${w.stitch?"checked":""}> Stitch each chapter</div></label>
        <label class="fact"><div class="k">Single book</div><div class="v"><input id="a_m4b" type="checkbox" ${w.m4b?"checked":""} ${App.options.ffmpeg_available?"":"disabled"}> Create M4B${App.options.ffmpeg_available?"":" (ffmpeg unavailable)"}</div></label>
      </div>
      <div class="wizfoot" style="padding:22px 0 0"><span class="launchstatus" id="audio-status">${esc(w.status||"")}</span><span class="grow"></span>
        <button class="btn btn-primary" id="audio-launch" onclick="bfLaunchAudiobook()">Start audiobook</button></div>
    </div></div>`;
}

async function bfLaunchAudiobook() {
  syncAudioInputs(); const w = App.audioWizard, p = audioProviderOption(w.provider);
  if (!w.file) return audioToast("choose an EPUB file");
  if (!w.model) return audioToast("model is required");
  if (p.requires_voice && !w.voice) return audioToast("ElevenLabs voice ID is required");
  if (w.launching) return; w.launching = true;
  const button = $("#audio-launch"); if (button) { button.disabled=true; button.textContent="Starting…"; }
  audioToast("uploading and planning…");
  const fd = new FormData(); fd.append("file", w.file); fd.append("provider", w.provider);
  fd.append("model", w.model); fd.append("voice", w.voice); fd.append("format", w.format);
  fd.append("speed", String(w.speed)); fd.append("max_chars", String(w.maxChars)); fd.append("concurrency", String(w.concurrency));
  if (w.instructions) fd.append("instructions", w.instructions); if (w.baseUrl) fd.append("base_url", w.baseUrl);
  if (w.apiKey) fd.append("api_key", w.apiKey); if (w.stitch) fd.append("stitch", "true"); if (w.m4b) fd.append("m4b", "true");
  try {
    const response = await fetch("/api/audiobook", {method:"POST", headers:{[CSRF_HEADER]:CSRF_TOKEN}, body:fd});
    const result = await response.json();
    if (!response.ok) { w.launching=false; if(button){button.disabled=false;button.textContent="Start audiobook";} return audioToast(result.error||"launch failed"); }
    await loadProviderStatus(); App.audioSelected = result.id; localStorage.setItem("bf-audiobook-id", result.id); renderAudiobook($("#stage"));
  } catch (error) { w.launching=false; if(button){button.disabled=false;button.textContent="Start audiobook";} audioToast("launch failed"); }
}

async function renderAudiobookProgress(stage, id) {
  stage.innerHTML = `<div class="wrap"><div class="pagehead"><div><h1>Audiobook progress</h1><p>Every completed chunk is durably checkpointed.</p></div><button class="btn btn-primary" onclick="bfStartAudiobook()">+ New audiobook</button></div><div class="wizpanel" id="audio-progress"><div class="empty">Loading…</div></div></div>`;
  await pollAudiobook(id);
}
async function pollAudiobook(id) {
  if (App.screen !== "audiobook" || App.audioSelected !== id) return;
  let data; try { const response=await fetch(`/api/audiobooks/${encodeURIComponent(id)}`); data=await response.json(); if(!response.ok) throw new Error(); }
  catch(error) { const panel=$("#audio-progress"); if(panel) panel.innerHTML=`<div class="empty">Could not load this audiobook operation.</div>`; return; }
  const chunks = data.chunks || [], done = data.completed_chunks || 0, total = chunks.length, progress = pct(done,total);
  const processStatus = data.process && data.process.status;
  let status = data.status || processStatus || "starting";
  if (status === "succeeded" && processStatus === "running") status = "stitching";
  const panel = $("#audio-progress"); if (!panel) return;
  panel.innerHTML = `<div class="facts">
      <div class="fact"><div class="k">Status</div><div class="v"><span class="badge ${badgeClass(status)}">${esc(status)}</span></div></div>
      <div class="fact"><div class="k">Provider</div><div class="v mono">${esc(data.synthesis_id||"planning")}</div></div>
      <div class="fact"><div class="k">Voice</div><div class="v">${esc(data.voice||"—")}</div></div>
      <div class="fact"><div class="k">Progress</div><div class="v mono">${done}/${total||"?"}</div></div>
    </div><div class="bar-track" style="margin:22px 0"><div class="bar-fill" style="width:${progress}%"></div></div>
    <div class="costbox"><div><div class="ck">Output</div><div class="mono" style="margin-top:8px;word-break:break-all">${esc(data.artifact||data.out_dir||"")}</div></div><div class="cm">${progress}% complete<br>${num(chunks.reduce((sum,chunk)=>sum+(chunk.chars||0),0))} characters planned</div></div>
    <div class="wizfoot" style="padding:18px 0 0"><span class="grow"></span>
      ${!["succeeded","failed","cancelled"].includes(status)?`<button class="btn btn-ghost" onclick="bfCancelAudiobook('${esc(id)}')">Cancel</button>`:""}
      ${status==="succeeded"?`<a class="btn btn-primary" href="/api/audiobooks/${encodeURIComponent(id)}/artifact">Download ${data.artifact?"M4B":"audio ZIP"}</a>`:""}
    </div>${data.error?`<div class="empty" style="color:var(--bad)">${esc(data.error)}</div>`:""}`;
  if (!["succeeded","failed","cancelled"].includes(status)) setTimeout(()=>pollAudiobook(id), 800);
}
async function bfCancelAudiobook(id) {
  try {
    const response = await fetch(`/api/audiobooks/${encodeURIComponent(id)}/cancel`, {method:"POST", headers:{[CSRF_HEADER]:CSRF_TOKEN}});
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || "cancel failed");
    pollAudiobook(id);
  } catch (error) {
    const panel=$("#audio-progress"); if(panel) panel.insertAdjacentHTML("beforeend", `<div class="empty" style="color:var(--bad)">${esc(error.message||"cancel failed")}</div>`);
  }
}

/* ---------------- Progress ---------------- */
async function renderProgress(stage) {
  const id = App.selected;
  if (!id) { stage.innerHTML = `<div class="wrap"><div class="empty">Open a translation from the library to watch its progress.</div></div>`; return; }
  stage.innerHTML = `<div class="wrap"><div class="empty">Loading job…</div></div>`;
  let d, runtime = null;
  try {
    const [jobResponse, runtimeResponse] = await Promise.all([
      fetch("/api/jobs/" + encodeURIComponent(id)),
      fetch("/api/jobs/" + encodeURIComponent(id) + "/reconfigure"),
    ]);
    if (!jobResponse.ok) throw new Error();
    d = await jobResponse.json();
    if (runtimeResponse.ok) runtime = await runtimeResponse.json();
  }
  catch (e) { stage.innerHTML = `<div class="wrap"><div class="empty">Could not load this job.</div></div>`; return; }
  App.runtimeSettings = runtime;
  App.runtimeJob = id;
  const title = titleFromPath(d.input_path);
  stage.innerHTML = `<div class="wrap">
    <div class="prog-hero"><div class="prog-cover">${esc(title.charAt(0))}</div>
      <div class="h"><div class="t">${esc(d.id)}</div>
        <div class="m">${esc(d.provider)} / ${esc(d.model)} · ${esc(d.source_lang||"auto")} → ${esc(d.target_lang)}</div></div>
      <span class="badge ${badgeClass(d.status)}" id="progpill">${esc(d.status)}</span></div>
    <div class="prog-card">
      <div class="prog-top"><div class="prog-pct"><span id="pctv">0</span><small>%</small></div>
        <div class="prog-eta" id="etav"></div></div>
      <div class="prog-bar" id="progbar"><i id="barfill" style="width:0%"></i></div>
      <div class="prog-actions"><span class="live"><span class="dot" id="livedot"></span><span id="livetxt">connecting…</span></span>
        <span class="grow" style="flex:1"></span>
        <button class="btn btn-ghost" id="pausebtn" onclick="bfJobControl('${esc(d.id)}','pause')">Pause</button>
        <button class="btn btn-ghost" id="resumebtn" onclick="bfJobControl('${esc(d.id)}','resume')">Resume</button>
        <button class="btn btn-ghost" id="stopbtn" onclick="bfJobControl('${esc(d.id)}','stop')">Stop</button>
        <button class="btn btn-ghost" onclick="bfGo('review',{selected:'${esc(d.id)}'})">Open review →</button>
        <button class="btn btn-ghost" id="retrybtn" onclick="bfRetry('${esc(d.id)}')">Retry failed / needs-review</button></div></div>
    <div class="stat-grid" id="stats"></div>
    <div id="runtime-panel"></div>
    <p class="sectlabel">Live activity</p>
    <div class="logbox scr" id="events"><div class="logline">waiting…</div></div>
    <div style="margin-top:16px"><p class="sectlabel">Issues</p><div class="logbox scr" id="issues"><div class="logline">none</div></div></div>
    <span class="toast" id="toast" style="font:400 12px var(--sans);color:var(--muted)"></span>
    </div>`;
  drawRuntimeSettings(runtime);
  updateState(d.state || {});
  openStream(id);
}
function runtimeOption(value, current, label) { return `<option value="${esc(value)}" ${value===current?"selected":""}>${esc(label||value)}</option>`; }
function drawRuntimeSettings(view) {
  const panel = $("#runtime-panel"); if (!panel) return;
  if (!view) { panel.innerHTML = `<div class="prog-card"><div class="runtime-head"><div><div class="title">Runtime settings</div><div class="meta">No resumable run snapshot is available.</div></div></div></div>`; return; }
  const e = view.effective || {}, ident = view.identity || {}, lease = view.lease || {};
  const disabled = view.editable ? "" : "disabled";
  const boundaries = (view.next_boundary || []).map(v => v.replace(/_/g," ")).join(", ") || "none pending";
  const leaseNote = lease.state === "fresh" ? `worker ${lease.pid || "?"} - applied r${view.applied_revision || 0}` : `${lease.state || "missing"} worker - Resume required`;
  panel.innerHTML = `<div class="prog-card">
    <div class="runtime-head"><div><div class="title">Runtime settings</div><div class="meta">revision r${view.revision || 0} - ${esc(leaseNote)} - boundary: ${esc(boundaries)}</div></div><span class="runtime-state ${esc(lease.state || "missing")}">${esc(view.application_state || "resume_required")}</span></div>
    <div class="runtime-grid">
      <label class="runtime-field">Batch output tokens<input class="inp" id="rt-output" type="number" min="1" value="${e.batch_max_output_tokens == null ? "" : esc(e.batch_max_output_tokens)}" placeholder="provider default" ${disabled}></label>
      <label class="runtime-field">Batch max items<input class="inp" id="rt-items" type="number" min="1" value="${esc(e.batch_max_items)}" ${disabled}></label>
      <label class="runtime-field">Batch target tokens<input class="inp" id="rt-target" type="number" min="1" value="${esc(e.batch_target_tokens)}" ${disabled}></label>
      <label class="runtime-field">Concurrency<input class="inp" id="rt-concurrency" type="number" min="1" value="${esc(e.concurrency)}" ${disabled}></label>
      <label class="runtime-field">Provider attempts<input class="inp" id="rt-attempts" type="number" min="1" value="${esc(e.provider_max_attempts)}" ${disabled}></label>
      <label class="runtime-field">QA<select class="inp" id="rt-qa" ${disabled}>${runtimeOption("off",e.qa,"Off")}${runtimeOption("suspicious",e.qa,"Suspicious")}${runtimeOption("all",e.qa,"All")}</select></label>
      <label class="runtime-field">Double-check<select class="inp" id="rt-double" ${disabled}>${runtimeOption("Off",e.double_check,"Off")}${runtimeOption("Formatting",e.double_check,"Formatting")}${runtimeOption("Semantic",e.double_check,"Semantic")}${runtimeOption("Full",e.double_check,"Full")}</select></label>
      <label class="runtime-field"><span>Validation</span><span class="runtime-check"><input id="rt-validate" type="checkbox" ${e.validate_output?"checked":""} ${disabled}> Validate output</span></label>
      <label class="runtime-field"><span>Adaptive concurrency</span><span class="runtime-check"><input id="rt-adaptive-concurrency" type="checkbox" ${e.adaptive_concurrency?"checked":""} ${disabled}> Enabled</span></label>
      <label class="runtime-field"><span>Adaptive batch sizing</span><span class="runtime-check"><input id="rt-adaptive-batch" type="checkbox" ${e.adaptive_batch_sizing?"checked":""} ${disabled}> Enabled</span></label>
    </div>
    <div class="runtime-identity">Immutable identity: ${esc(ident.provider || "-")} / ${esc(ident.model || "-")} - ${esc(ident.source_language || "auto")} to ${esc(ident.target_language || "-")} - ${esc(ident.profile || "-")} - prompt ${esc(ident.prompt_version || "-")}</div>
    <div class="runtime-foot"><button class="btn btn-primary" id="runtime-save" onclick="bfSaveRuntimeSettings()" ${disabled}>Save runtime settings</button><span class="runtime-feedback" id="runtime-feedback">${view.editable ? "Changes apply at the named request, batch, or stage boundary." : (view.resumable_work ? "This job is not in an editable state." : "No resumable work remains.")}</span></div>
  </div>`;
}
function runtimePositiveInt(id, label, optional) {
  const el = $(id), raw = el ? el.value.trim() : "";
  if (optional && raw === "") return null;
  const value = Number(raw);
  if (!Number.isInteger(value) || value < 1) throw new Error(`${label} must be a positive integer`);
  return value;
}
async function bfSaveRuntimeSettings() {
  const view = App.runtimeSettings, feedback = $("#runtime-feedback"), button = $("#runtime-save");
  if (!view || !view.editable) return;
  let values;
  try {
    values = {
      batch_max_output_tokens: runtimePositiveInt("#rt-output","batch output tokens",true),
      batch_max_items: runtimePositiveInt("#rt-items","batch max items",false),
      batch_target_tokens: runtimePositiveInt("#rt-target","batch target tokens",false),
      concurrency: runtimePositiveInt("#rt-concurrency","concurrency",false),
      provider_max_attempts: runtimePositiveInt("#rt-attempts","provider attempts",false),
      qa: $("#rt-qa").value,
      double_check: $("#rt-double").value,
      validate_output: $("#rt-validate").checked,
      adaptive_concurrency: $("#rt-adaptive-concurrency").checked,
      adaptive_batch_sizing: $("#rt-adaptive-batch").checked,
    };
  } catch (e) { if (feedback) { feedback.textContent=e.message; feedback.classList.add("bad"); } return; }
  const payload = {}, previous = view.effective || {};
  Object.keys(values).forEach(key => { if (values[key] !== previous[key] && !(key === "batch_max_output_tokens" && values[key] === null)) payload[key] = values[key]; });
  if (!Object.keys(payload).length) { if (feedback) { feedback.textContent="No runtime settings changed."; feedback.classList.remove("bad"); } return; }
  if (button) button.disabled = true;
  if (feedback) { feedback.textContent="Saving..."; feedback.classList.remove("bad"); }
  try {
    const r = await fetch(`/api/jobs/${encodeURIComponent(App.selected)}/reconfigure`, {method:"POST",headers:{"Content-Type":"application/json",[CSRF_HEADER]:CSRF_TOKEN},body:JSON.stringify(payload)});
    const body = await r.json(); if (!r.ok) throw new Error(body.error || "runtime update failed");
    App.runtimeSettings = body; drawRuntimeSettings(body);
    const next = (body.next_boundary || []).map(v => v.replace(/_/g," ")).join(", ");
    const out = $("#runtime-feedback"); if (out) out.textContent = `Saved revision r${body.revision} - ${body.live ? (next || "live") : "Resume required"}`;
  } catch (e) { if (feedback) { feedback.textContent=e.message || "runtime update failed"; feedback.classList.add("bad"); } }
  finally { const current=$("#runtime-save"); if (current) current.disabled=false; }
}
async function refreshRuntimeSettings() {
  const id = App.selected;
  if (!id || App.runtimeRefreshPending || App.screen !== "progress") return;
  App.runtimeRefreshPending = true;
  try {
    const r = await fetch(`/api/jobs/${encodeURIComponent(id)}/reconfigure`);
    if (r.ok && App.screen === "progress" && App.selected === id) { App.runtimeSettings = await r.json(); App.runtimeJob=id; drawRuntimeSettings(App.runtimeSettings); }
  } catch (_) {}
  finally { App.runtimeRefreshPending=false; }
}
function setLive(on, txt) { const dt = $("#livedot"), tx = $("#livetxt"); if (dt) dt.classList.toggle("on", on); if (tx) tx.textContent = txt; }
function updateState(s) {
  const total = s.total_segments || 0, done = s.done_segments || 0, p = pct(done, total);
  const liveStatus = s.paused ? "paused" : (s.finished ? "done" : null);
  if (liveStatus) setProgressStatus(liveStatus);
  const fill = $("#barfill"); if (fill) fill.style.width = p + "%";
  const pv = $("#pctv"); if (pv) pv.textContent = p;
  const bar = $("#progbar"); if (bar) bar.classList.toggle("done", !!s.finished);
  const etav = $("#etav"); if (etav) etav.innerHTML = `${done} / ${total} segments<br>${s.finished ? "Finished" : "about " + fmtDur(etaSecs(s)) + " remaining"}`;
  const stats = [
    ["done", num(done), ""], ["succeeded", num(s.succeeded || 0), "good"], ["cached", num(s.cached || 0), ""],
    ["needs review", num(s.needs_review || 0), s.needs_review ? "warn" : ""],
    ["failed", num(s.failed || 0), s.failed ? "bad" : ""],
    ["active", `${s.active_requests || 0}/${s.target_concurrency || 0}`, ""],
    ["seg/min", segPerMin(s).toFixed(1), ""], ["elapsed", fmtDur(elapsedSecs(s)), ""],
    ["tokens in", num(s.input_tokens), ""], ["tokens out", num(s.output_tokens), ""],
  ];
  const box = $("#stats"); if (box) box.innerHTML = stats.map(([k,v,c]) => `<div class="stat"><div class="k">${esc(k)}</div><div class="v ${c}">${esc(v)}</div></div>`).join("");
  const ibox = $("#issues");
  if (ibox) { const issues = s.recent_issues || [];
    ibox.innerHTML = issues.length ? issues.slice().reverse().map(i => `<div class="logline ${i.level==="Error"?"bad":"warn"}">${i.level==="Error"?"✗":"⚠"} ${esc(i.kind)}: ${esc(shorten(i.message,120))}</div>`).join("") : `<div class="logline">none</div>`;
  }
  const ebox = $("#events");
  if (ebox) { const evs = s.recent_events || [];
    ebox.innerHTML = evs.length ? evs.slice().reverse().map(fmtEvent).join("") : `<div class="logline">waiting…</div>`;
  }
  const runtimeRevision = Number(s.runtime_config_revision || 0);
  if (runtimeRevision && (!App.runtimeSettings || runtimeRevision > Number(App.runtimeSettings.applied_revision || 0))) refreshRuntimeSettings();
  if (s.runtime_config_rejection) {
    const feedback = $("#runtime-feedback");
    if (feedback) { feedback.textContent = `Rejected: ${s.runtime_config_rejection}`; feedback.classList.add("bad"); }
  }
}
function fmtEvent(ev) {
  const key = Object.keys(ev)[0]; const v = ev[key] || {}; let cls = "", body = key;
  switch (key) {
    case "SegmentFinished": body = `segment ${shorten(v.segment_id,18)} → ${v.status}`; if (v.status==="failed") cls="bad"; else if (v.status==="needs_review") cls="warn"; break;
    case "SegmentStarted": body = `segment ${shorten(v.segment_id,18)} started`; break;
    case "RequestStarted": { const audit = [v.runtime_config_revision == null ? null : `r${v.runtime_config_revision}`, v.provider_max_attempts == null ? null : `${v.provider_max_attempts} attempts`].filter(Boolean).join(" - "); body = `request started (${v.active_requests}/${v.target_concurrency})${audit ? " - " + audit : ""}`; break; }
    case "RequestFinished": body = `request ${v.status} · ${v.latency_ms}ms`; if (v.status!=="ok"&&v.status!=="succeeded") cls="warn"; break;
    case "StageStarted": body = `stage: ${v.stage}`; break;
    case "StageFinished": body = `stage complete: ${v.stage}`; break;
    case "SegmentationFinished": body = `segmented into ${v.segment_count} segments`; break;
    case "CacheScanFinished": body = `cache scan: ${v.hits} hits / ${v.misses} misses`; break;
    case "JobPaused": body = `paused`; cls="warn"; break;
    case "JobResumed": body = `resumed`; cls="good"; break;
    case "CheckpointFlushed": body = `checkpoint flushed (${v.flushed_count})`; break;
    case "ConcurrencyChanged": body = `concurrency ${v.previous} → ${v.current} (${v.reason})`; break;
    case "RuntimeConfigChanged": body = `runtime r${v.revision}: ${(v.changed_fields || []).join(", ") || "updated"} -> ${(v.application || []).join(", ") || "next boundary"}`; cls="good"; break;
    case "RuntimeConfigRejected": body = `runtime config rejected${v.revision == null ? "" : " r" + v.revision}: ${shorten(v.message || "invalid settings",90)}`; cls="bad"; break;
    case "Warning": body = `⚠ ${v.kind}: ${shorten(v.message,90)}`; cls="warn"; break;
    case "Error": body = `✗ ${v.kind}: ${shorten(v.message,90)}`; cls="bad"; break;
    case "TranslationFinished": body = `finished: ${v.succeeded} ok, ${v.cached} cached, ${v.needs_review} review, ${v.failed} failed`; cls="good"; break;
  }
  return `<div class="logline ${cls}">${esc(body)}</div>`;
}
function openStream(id) {
  closeStream();
  App.es = new EventSource("/api/jobs/" + encodeURIComponent(id) + "/events");
  setLive(true, "live");
  App.es.addEventListener("state", (e) => { if (App.selected === id && App.screen === "progress") { try { updateState(JSON.parse(e.data)); } catch (_) {} } });
  App.es.addEventListener("done", () => { setLive(false, "finished"); closeStream(); });
  App.es.onerror = () => setLive(false, "reconnecting…");
}
function closeStream() { if (App.es) { App.es.close(); App.es = null; } }
async function bfRetry(id) {
  const btn = $("#retrybtn"), toast = $("#toast");
  if (btn) btn.disabled = true; if (toast) toast.textContent = "submitting…";
  try {
    const r = await fetch("/api/jobs/" + encodeURIComponent(id) + "/retry", { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN } });
    const j = await r.json();
    if (toast) toast.textContent = r.ok ? `marked ${j.retried} segment(s) — run: bookforge resume ${id}` : (j.error || "retry failed");
  } catch (e) { if (toast) toast.textContent = "retry failed"; }
  if (btn) btn.disabled = false;
}
function setProgressStatus(status) {
  const pill = $("#progpill");
  if (!pill) return;
  pill.textContent = status;
  pill.className = "badge " + badgeClass(status);
}
async function bfJobControl(id, command) {
  const toast = $("#toast");
  const buttons = ["#pausebtn","#resumebtn","#stopbtn"].map(id => $(id)).filter(Boolean);
  buttons.forEach(b => b.disabled = true);
  if (toast) toast.textContent = command + " requested…";
  try {
    const r = await fetch("/api/jobs/" + encodeURIComponent(id) + "/" + command, { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN } });
    const j = await r.json();
    if (r.ok) {
      if (command === "pause") setProgressStatus("paused");
      if (command === "resume") setProgressStatus("running");
      if (command === "stop") setProgressStatus("stopped");
      let message = `${command} requested`;
      if (command === "resume" && j.mode === "signaled") message = "Resume signaled to the live worker.";
      if (command === "resume" && j.mode === "spawned") message = `Resume worker started${j.pid ? " (PID " + j.pid + ")" : ""}.`;
      if (command === "resume" && j.mode === "launching") message = "A resume worker is already starting.";
      if (toast) toast.textContent = message;
      setTimeout(refreshRuntimeSettings, 350);
    } else {
      if (toast) toast.textContent = j.error || `${command} failed`;
    }
  } catch (e) {
    if (toast) toast.textContent = `${command} failed`;
  }
  buttons.forEach(b => b.disabled = false);
}

/* ---------------- Review / Validation / Glossary (wired in later milestones) ---------------- */
function placeholder(stage, title, note) {
  stage.innerHTML = `<div class="wrap"><div class="pagehead"><div><h1>${title}</h1><p>${note}</p></div></div>
    <div class="empty">${App.selected ? "Loading…" : "Open a job from the library first."}</div></div>`;
}
function segTag(seg, flagged) {
  if (flagged) return { label:"Flagged", cls:"bad" };
  if (seg.human_corrected) return { label:"corrected", cls:"" };
  if (seg.status === "failed") return { label:"failed", cls:"bad" };
  if (seg.status === "needs_review") return { label:"review", cls:"warn" };
  if ((seg.soft_warnings || []).length) return { label:"check", cls:"warn" };
  return { label:"ok", cls:"" };
}
async function renderReview(stage) {
  const id = App.selected;
  if (!id) { placeholder(stage, "Review", "Side-by-side source and translation."); return; }
  stage.innerHTML = `<div class="review"><div class="rev-empty">Loading review…</div></div>`;
  let doc;
  try {
    const r = await fetch("/api/jobs/" + encodeURIComponent(id) + "/review");
    doc = await r.json();
    if (!r.ok) { stage.innerHTML = `<div class="wrap"><div class="empty">${esc(doc.error || "Review is not available for this job.")}</div></div>`; return; }
  } catch (e) { stage.innerHTML = `<div class="wrap"><div class="empty">Could not load review.</div></div>`; return; }
  App.review = { doc, idx: 0, filter: "all", hintOpen: false, hintText: "", notice: "" };
  drawReview();
}
function bfReviewPick(i) { App.review.idx = i; App.review.hintOpen=false; App.review.hintText=""; App.review.notice=""; drawReview(); }
function bfReviewNav(d) { const n = (App.review.doc.segments || []).length; App.review.idx = Math.max(0, Math.min(n - 1, App.review.idx + d)); App.review.hintOpen=false; App.review.hintText=""; App.review.notice=""; drawReview(); }
function bfReviewFilter(f) { App.review.filter = f; drawReview(); }
async function bfReviewFlag() {
  const R = App.review, seg = R.doc.segments[R.idx]; if (!seg) return;
  const next = !seg.flagged;
  try {
    const r = await fetch(`/api/jobs/${encodeURIComponent(App.selected)}/segments/${encodeURIComponent(seg.segment_id)}/flag`, {
      method:"POST", headers:{"Content-Type":"application/json",[CSRF_HEADER]:CSRF_TOKEN}, body:JSON.stringify({flagged:next})
    });
    const body = await r.json(); if (!r.ok) throw new Error(body.error || "flag update failed");
    seg.flagged = next; drawReview();
  } catch (e) { window.alert(e.message || "flag update failed"); }
}
async function bfReviewSave() {
  const R = App.review, seg = R && R.doc.segments[R.idx]; if (!seg) return;
  const status = $("#rev-save-status"), button = $("#rev-save");
  const blocks = Array.from(document.querySelectorAll(".rev-edit")).map(el => ({ block_id: el.dataset.blockId, text: el.value }));
  if (blocks.some(block => !block.text.trim())) { if (status) status.textContent = "every block needs translation text"; return; }
  if (button) button.disabled = true; if (status) status.textContent = "saving and rebuilding…";
  try {
    const r = await fetch(`/api/jobs/${encodeURIComponent(App.selected)}/segments/${encodeURIComponent(seg.segment_id)}/translation`, {
      method: "POST", headers: { "Content-Type":"application/json", [CSRF_HEADER]: CSRF_TOKEN }, body: JSON.stringify({ blocks })
    });
    const body = await r.json();
    if (!r.ok) throw new Error(body.error || "correction failed");
    if (status) status.textContent = `saved · ${body.job_status}`;
    const refreshed = await fetch("/api/jobs/" + encodeURIComponent(App.selected) + "/review");
    R.doc = await refreshed.json(); drawReview();
  } catch (e) { if (status) status.textContent = e.message || "correction failed"; }
  finally { if (button) button.disabled = false; }
}
function bfReviewRetry() {
  const R = App.review, seg = R && R.doc.segments[R.idx]; if (!seg || seg.human_corrected) return;
  R.hintOpen = true;
  const panel = $("#rev-hint-panel"), input = $("#rev-hint-text");
  if (panel) panel.hidden = false;
  if (input) { input.value = R.hintText || ""; input.focus(); }
}
function bfReviewRetryCancel() {
  const R = App.review; if (!R) return;
  R.hintOpen = false; R.hintText = "";
  const panel = $("#rev-hint-panel"); if (panel) panel.hidden = true;
}
async function bfReviewStopForRetry() {
  const status = $("#rev-save-status"); if (status) status.textContent = "requesting stop…";
  try {
    const r = await fetch(`/api/jobs/${encodeURIComponent(App.selected)}/stop`, {method:"POST",headers:{[CSRF_HEADER]:CSRF_TOKEN}});
    const body = await r.json(); if (!r.ok) throw new Error(body.error || "stop failed");
    if (status) status.textContent = "Stop requested. Wait for the worker to stop, then queue the retry.";
  } catch (e) { if (status) status.textContent = e.message || "stop failed"; }
}
async function bfReviewRetrySubmit() {
  const R = App.review, seg = R && R.doc.segments[R.idx]; if (!seg) return;
  const input = $("#rev-hint-text"), guidance = input ? input.value.trim() : "";
  R.hintText = guidance;
  const status = $("#rev-save-status"); if (status) status.textContent = "queuing segment retry…";
  try {
    const r = await fetch(`/api/jobs/${encodeURIComponent(App.selected)}/segments/${encodeURIComponent(seg.segment_id)}/retry`, {
      method:"POST", headers:{"Content-Type":"application/json",[CSRF_HEADER]:CSRF_TOKEN}, body:JSON.stringify({guidance})
    });
    const body = await r.json(); if (!r.ok) throw new Error(body.error || "retry request failed");
    seg.status = "retry_pending"; R.hintOpen=false; R.hintText=""; R.notice=`Retry queued. Resume ${App.selected}.`; drawReview();
  } catch (e) { if (status) status.textContent = e.message || "retry request failed"; }
}
function drawReview() {
  const R = App.review, doc = R.doc, segs = doc.segments || [];
  const flaggedCount = segs.filter(seg => seg.flagged).length;
  const visible = segs.map((s, i) => ({ s, i })).filter(({ s }) => {
    if (R.filter === "flagged") return !!s.flagged;
    if (R.filter === "warnings") return (s.soft_warnings || []).length || (s.status !== "succeeded" && s.status !== "skipped_cached");
    return true;
  });
  const filters = [["all", `All ${segs.length}`], ["warnings", "To check"], ["flagged", `Flagged ${flaggedCount}`]];
  const rows = visible.map(({ s, i }) => {
    const flagged = !!s.flagged;
    const tag = segTag(s, flagged);
    const ref = `${s.chapter_title || s.chapter_id} ¶${s.ordinal}`;
    return `<div class="rev-row ${i === R.idx ? "on" : ""}" onclick="bfReviewPick(${i})">
      <div class="r"><span class="ref">${esc(shorten(ref, 24))}</span><span class="rev-tag ${tag.cls}">${tag.label}</span></div>
      <div class="prev">${esc(shorten(s.target_text || "—", 150))}</div></div>`;
  }).join("") || `<div style="padding:18px;color:var(--faint);font-size:12px">Nothing here.</div>`;
  const cur = segs[R.idx];
  const title = doc.source_book_title || titleFromPath(App.selected);
  const langs = `${esc(doc.source_language || "auto")} → ${esc(doc.target_language)}`;
  let main;
  if (!cur) {
    main = `<div class="rev-empty">No translated segments yet.</div>`;
  } else {
    const flagged = !!cur.flagged;
    const ref = `${cur.chapter_title || cur.chapter_id} ¶${cur.ordinal}`;
    const notes = (cur.soft_warnings || []).map(w =>
      `<div class="rev-note"><b>⚑ ${esc((w.kind || "note").replace(/_/g, " "))}</b> — ${esc(w.message || "")}</div>`).join("");
    const blockEditors = (cur.blocks || []).map(block => `<div class="rev-block"><div class="rev-block-id">${esc(block.block_id)}</div><textarea class="rev-edit" data-block-id="${esc(block.block_id)}">${esc(block.target_text || "")}</textarea></div>`).join("");
    const hintPanel = `<div class="rev-hint" id="rev-hint-panel" ${R.hintOpen ? "" : "hidden"}><label for="rev-hint-text">Guidance for the next translation attempt (optional)</label><textarea id="rev-hint-text" oninput="App.review.hintText=this.value" placeholder="Explain terminology, tone, or a specific correction.">${esc(R.hintText || "")}</textarea><div class="note">Stop the job before queuing a retry. Running and paused jobs reject retry changes.</div><div class="actions"><button class="btn btn-primary" onclick="bfReviewRetrySubmit()">Queue retry</button><button class="btn btn-ghost" onclick="bfReviewRetryCancel()">Cancel</button><button class="btn btn-ghost" onclick="bfReviewStopForRetry()">Stop job</button></div></div>`;
    main = `<div class="rev-bar"><span class="ref">${esc(ref)} · ${esc(cur.status)}${cur.human_corrected ? " · manual" : ""}</span>
        <div class="rev-nav"><button class="rev-flag ${flagged ? "on" : ""}" onclick="bfReviewFlag()">⚑ ${flagged ? "Flagged" : "Flag"}</button>
          <button class="rev-btn" onclick="bfReviewNav(-1)">←</button><button class="rev-btn" onclick="bfReviewNav(1)">→</button></div></div>
      <div class="rev-cols scr">
        <div class="rev-col"><div class="cl">Source · ${esc(doc.source_language || "auto")}</div><div class="rev-text">${esc(cur.source_text)}</div></div>
        <div class="rev-col tgt"><div class="cl">Translation · ${esc(doc.target_language)}</div>${blockEditors}<div class="rev-save-row"><button class="btn btn-primary" id="rev-save" onclick="bfReviewSave()">Save & rebuild</button><button class="btn btn-ghost" onclick="bfReviewRetry()" ${cur.human_corrected ? "disabled" : ""}>Re-translate with hint</button><span class="rev-save-status" id="rev-save-status">${R.notice || (cur.human_corrected ? "human correction saved" : "")}</span></div>${hintPanel}${notes}</div>
      </div>`;
  }
  $("#stage").innerHTML = `<div class="review">
    <div class="rev-list"><div class="lh"><div class="t">${esc(shorten(title, 32))}</div>
        <div class="m">${langs} · ${segs.length} segments · ${fmtCost(doc.totals && doc.totals.estimated_cost_usd)}</div>
        <div class="rev-filters">${filters.map(([f, l]) => `<div class="rev-filter ${R.filter === f ? "on" : ""}" onclick="bfReviewFilter('${f}')">${esc(l)}</div>`).join("")}</div></div>
      <div class="rev-rows scr">${rows}</div></div>
    <div class="rev-main">${main}</div></div>`;
}
async function renderValidation(stage) {
  const id = App.selected;
  if (!id) { placeholder(stage, "Validation", "EPUBCheck and structural validators."); return; }
  App.validation = App.validation || {};
  if (App.validation[id]) { drawValidation(App.validation[id]); } else { runValidation(); }
}
async function runValidation() {
  const id = App.selected;
  $("#stage").innerHTML = `<div class="wrap"><div class="empty">Running validators…</div></div>`;
  try {
    const r = await fetch("/api/jobs/" + encodeURIComponent(id) + "/validate", { method: "POST", headers: { [CSRF_HEADER]: CSRF_TOKEN } });
    const j = await r.json();
    if (!r.ok) { $("#stage").innerHTML = `<div class="wrap"><div class="empty">${esc(j.error || "Validation could not run.")}</div></div>`; return; }
    (App.validation = App.validation || {})[id] = j;
    if (App.screen === "validation" && App.selected === id) drawValidation(j);
  } catch (e) { $("#stage").innerHTML = `<div class="wrap"><div class="empty">Could not run validation.</div></div>`; }
}
function bfRevalidate() { runValidation(); }
function sevRank(s) { return s === "fatal" || s === "error" ? "bad" : s === "warning" ? "warn" : s === "info" ? "info" : "good"; }
function sevGlyph(cls) { return cls === "bad" ? "✗" : cls === "warn" ? "!" : cls === "info" ? "i" : "✓"; }
function drawValidation(rep) {
  const ec = rep.epubcheck || {}, bf = rep.bookforge_validators || {};
  const msgs = [...(bf.messages || []).map(m => ({ ...m, src: "BookForge" })), ...(ec.messages || []).map(m => ({ ...m, src: "EPUBCheck" }))];
  const errors = msgs.filter(m => m.severity === "error" || m.severity === "fatal").length;
  const warnings = msgs.filter(m => m.severity === "warning").length;
  const overall = errors ? "bad" : warnings ? "warn" : "good";
  const title = errors ? "Validation failed" : warnings ? `Passed with ${warnings} warning${warnings === 1 ? "" : "s"}` : "Passed";
  const ecUnavailable = ec.status === "unavailable";
  const sub = ec.ran ? `EPUBCheck ${esc(ec.version || "")} · ${esc(ec.status)}. BookForge validators: ${esc(bf.status || "-")}.`
    : `EPUBCheck not run. BookForge validators: ${esc(bf.status || "-")}.`;
  const note = ecUnavailable ? `<div class="val-note">EPUBCheck is unavailable — install <b>epubcheck</b> on PATH or set <b>BOOKFORGE_EPUBCHECK</b> to include the reader-compatibility pass. BookForge's own structural validators still ran.</div>` : "";
  const rows = msgs.length ? msgs.map(m => {
    const cls = sevRank(m.severity);
    return `<div class="val-item"><span class="val-dot ${cls}">${sevGlyph(cls)}</span>
      <div class="m"><div class="mt">${esc(m.text || m.code || "message")}</div>
        <div class="ml">${esc(m.src)} · ${esc(m.code || "")}${m.location ? " · " + esc(m.location) : ""}</div></div></div>`;
  }).join("") : `<div class="val-item"><span class="val-dot good">✓</span><div class="m"><div class="mt">No issues reported.</div></div></div>`;
  $("#stage").innerHTML = `<div class="wrap">
    <div class="val-hero"><div class="val-icon ${overall}">${overall === "bad" ? "✗" : overall === "warn" ? "!" : "✓"}</div>
      <div class="h"><div class="t">${title}</div><div class="s">${sub}</div></div>
      <button class="btn btn-ghost" onclick="bfRevalidate()">Re-run check</button></div>
    ${note}
    <div class="val-stats">
      <div class="val-stat"><div class="v ${errors ? "bad" : "good"}">${errors}</div><div class="l">Errors</div></div>
      <div class="val-stat"><div class="v ${warnings ? "warn" : "good"}">${warnings}</div><div class="l">Warnings</div></div>
      <div class="val-stat"><div class="v">${bf.xml_valid ? "OK" : "—"}</div><div class="l">Structure${bf.files_checked != null ? " · " + bf.files_checked + " files" : ""}</div></div>
    </div>
    <p class="sectlabel">Validator messages</p>
    <div class="val-list">${rows}</div></div>`;
}
const GL_CATEGORIES = ["person","place","object","invented","style","phrase","other"];
function renderGlossary(stage) {
  if (!App.glossary) {
    const langs = App.options.languages || [];
    const to = langs.includes("Italian") ? "Italian" : (langs.find(l => l !== "English") || "Italian");
    App.glossary = { from: "English", to, terms: [] };
  }
  const g = App.glossary;
  const langOpts = (App.options.languages || []).map(l => `<option value="${esc(l)}">`).join("");
  stage.innerHTML = `<div class="wrap">
    <div class="pagehead"><div><h1>Glossary</h1><p>Lock names, places and recurring terms to a fixed translation. Applied across every chapter for consistency.</p></div></div>
    <div class="gl-langs">
      <div><div class="field-label">From</div><input class="inp" id="gl_from" list="gllangs" value="${esc(g.from)}"></div>
      <span style="align-self:end;padding-bottom:12px;color:var(--faint)">→</span>
      <div><div class="field-label">Into</div><input class="inp" id="gl_to" list="gllangs" value="${esc(g.to)}"></div>
      <button class="btn btn-ghost" style="align-self:end" onclick="bfGlossaryReload()">Show</button>
    </div>
    <datalist id="gllangs">${langOpts}</datalist>
    <div class="gl-add">
      <input class="inp" id="gl_src" placeholder="Source term (e.g. the Grange)">
      <span style="color:var(--faint)">→</span>
      <input class="inp" id="gl_tgt" placeholder="Translation (e.g. la Grange)">
      <select class="inp" id="gl_cat" style="max-width:140px">${GL_CATEGORIES.map(c => `<option value="${c}">${c}</option>`).join("")}</select>
      <button class="btn btn-primary" style="white-space:nowrap;padding:11px 18px" onclick="bfGlossaryAdd()">Add term</button>
    </div>
    <div class="gl-status" id="gl_status"></div>
    <div class="gl-table" id="gl_table"><div class="empty" style="margin-top:20px">Loading…</div></div></div>`;
  loadGlossary();
}
async function loadGlossary() {
  const g = App.glossary;
  const q = `?source=${encodeURIComponent(g.from)}&target=${encodeURIComponent(g.to)}&scope=global`;
  let terms = [];
  try { terms = await (await fetch("/api/glossary" + q)).json(); } catch (e) { terms = []; }
  g.terms = Array.isArray(terms) ? terms : [];
  drawGlossaryTable();
}
function drawGlossaryTable() {
  const g = App.glossary, box = $("#gl_table"); if (!box) return;
  if (!g.terms.length) { box.innerHTML = `<div class="empty" style="margin-top:20px">No terms for ${esc(g.from)} → ${esc(g.to)} yet.</div>`; return; }
  const rows = g.terms.map(t => `<div class="gl-row">
    <div class="gl-c s">${esc(t.source)}</div><div class="gl-c t">${esc(t.target)}</div>
    <div class="gl-c cat">${esc(t.category || "")}</div>
    <div class="gl-c x" title="Remove" onclick="bfGlossaryRemove(${Number(t.id)})">×</div></div>`).join("");
  box.innerHTML = `<div class="gl-head"><div>Source</div><div>Translation</div><div>Category</div><div></div></div>${rows}
    <div class="gl-foot">${g.terms.length} term${g.terms.length === 1 ? "" : "s"} · global scope · ${esc(g.from)} → ${esc(g.to)}</div>`;
}
function bfGlossaryReload() {
  const f = $("#gl_from"), t = $("#gl_to");
  if (f) App.glossary.from = f.value.trim() || "English";
  if (t) App.glossary.to = t.value.trim() || "Italian";
  loadGlossary();
}
async function bfGlossaryAdd() {
  const g = App.glossary, status = $("#gl_status");
  const source = $("#gl_src").value.trim(), target = $("#gl_tgt").value.trim(), category = $("#gl_cat").value;
  if (!source || !target) { status.textContent = "enter a source term and its translation"; return; }
  status.textContent = "saving…";
  try {
    const r = await fetch("/api/glossary", {
      method: "POST",
      headers: { [CSRF_HEADER]: CSRF_TOKEN, "content-type": "application/json" },
      body: JSON.stringify({ source, target, category, scope: "global", source_language: g.from, target_language: g.to, always_active: true }),
    });
    const j = await r.json();
    if (!r.ok) { status.textContent = j.error || "could not add term"; return; }
    status.textContent = ""; $("#gl_src").value = ""; $("#gl_tgt").value = "";
    loadGlossary();
  } catch (e) { status.textContent = "could not add term"; }
}
async function bfGlossaryRemove(id) {
  try { await fetch("/api/glossary/" + id, { method: "DELETE", headers: { [CSRF_HEADER]: CSRF_TOKEN } }); loadGlossary(); } catch (e) {}
}

/* ---------------- boot ---------------- */
async function loadOptions() {
  try { const r = await fetch("/api/options"); if (r.ok) App.options = await r.json(); } catch (e) {}
}
async function loadProviderStatus() {
  try { App.providerKeys = await (await fetch("/api/providers")).json(); } catch (e) { App.providerKeys = {}; }
}
async function boot() {
  applyTheme();
  App.audioSelected = localStorage.getItem("bf-audiobook-id");
  await Promise.all([loadOptions(), loadProviderStatus()]);
  render();
  // Keep the library list live while it's on screen (statuses/progress advance).
  setInterval(() => { if (App.screen === "library") loadLibraryJobs(); }, 4000);
}
boot();
