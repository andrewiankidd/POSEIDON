// Runtime environment container for the browser (the frontend can't read backend
// env vars). This file is DELIBERATELY empty: it only exists so the <script> tag
// in app.html has something to load on no-server hosting (GitHub Pages, the Tauri
// webview), where there's no backend to serve a dynamic /env.js.
//
// The keys themselves are defined in exactly ONE place - the server's `/env.js`
// handler (crates/poseiden-server/src/http.rs), which resolves them from env vars
// at request time and whose route overrides this static file. Consumers read
// `window.__POSEIDEN_ENV__?.<key>` and default when absent, so nothing is
// duplicated here. Add a new runtime setting in the server handler, not here.
window.__POSEIDEN_ENV__ = {};
