// Sketchpad's browser bridge. Agents (sketchpad's notebook agent, Claude,
// scripts) reach this browser through `sketchpad browser …`, which talks to
// the native host Chromium starts for this extension (`sketchpad` itself).
//
// Everything happens in background tabs, gathered in a collapsed "Agent" tab
// group: your tab and your keyboard focus never move, except when an agent
// shows you a tab (`show`) so you can do something there yourself.
//
// The agent reads any tab, but acts only in tabs it opened. Consequential
// actions (submitting a form, buttons like pay / send / post / delete) come
// back as {needs_confirmation} unless the request says confirmed: the caller
// gets that from the human. It never types into password fields.
//
//   host -> extension  {"id":1,"cmd":"search","q":"…","engine":"google"|"duckduckgo","n":8}
//                      {"id":2,"cmd":"read","url":"…","max":20000}
//                      {"id":3,"cmd":"tabs"}
//                      {"id":4,"cmd":"tab","tabId":123,"max":20000}
//                      {"id":5,"cmd":"ping"}  {"id":6,"cmd":"reload"}
//                      {"cmd":"open","url":"…","show":false}          a tab the agent keeps; returns a snapshot
//                      {"cmd":"snapshot","tabId":1}                   text + numbered elements (refs)
//                      {"cmd":"act","tabId":1,"action":"click"|"type"|"select"|"key"|"scroll",
//                       "ref":"e12","text":"…","submit":true,"value":"…","key":"Enter","dy":600,"confirmed":false}
//                      {"cmd":"navigate","tabId":1,"url":"…"}  {"cmd":"back","tabId":1}
//                      {"cmd":"show","tabId":1}  {"cmd":"close","tabId":1}
//   extension -> host  {"id":1,"ok":true,"result":…} | {"id":1,"ok":false,"error":"…"}

const HOST = "dev.sketchpad.bridge";
const MAX_TEXT = 20000;
const SNAPSHOT_TEXT = 6000;
let port = null;

function connect() {
	if (port) return;
	try {
		port = chrome.runtime.connectNative(HOST);
	} catch (e) {
		port = null;
		return;
	}
	port.onMessage.addListener(async (msg) => {
		const reply = { id: msg.id };
		try {
			reply.result = await handle(msg);
			reply.ok = true;
		} catch (e) {
			reply.ok = false;
			reply.error = String(e?.message ?? e);
		}
		port?.postMessage(reply);
	});
	port.onDisconnect.addListener(() => {
		port = null;
	});
}

// Keep the host connected: on start, on install, and every minute in case it went away.
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
chrome.alarms.create("reconnect", { periodInMinutes: 1 });
chrome.alarms.onAlarm.addListener(connect);
connect();

async function handle(msg) {
	switch (msg.cmd) {
		case "ping":
			return { version: chrome.runtime.getManifest().version };
		case "reload":
			// Pick up changes to the unpacked extension (after replying).
			setTimeout(() => chrome.runtime.reload(), 100);
			return { reloading: true };
		case "search":
			return search(String(msg.q ?? ""), msg.engine ?? "google", msg.n ?? 8);
		case "read":
			return read(String(msg.url ?? ""), msg.max ?? MAX_TEXT);
		case "tabs": {
			const mine = await agentTabs();
			return (await chrome.tabs.query({})).map((t) => ({
				tabId: t.id,
				windowId: t.windowId,
				active: t.active,
				agent: mine.includes(t.id),
				title: t.title,
				url: t.url,
			}));
		}
		case "tab":
			return extract(Number(msg.tabId), msg.max ?? MAX_TEXT);
		case "open":
			return open(String(msg.url ?? ""), !!msg.show);
		case "snapshot":
			return snapshot(Number(msg.tabId), msg.max ?? SNAPSHOT_TEXT);
		case "act":
			return act(Number(msg.tabId), msg);
		case "navigate":
			return navigate(Number(msg.tabId), String(msg.url ?? ""));
		case "back":
			return goBack(Number(msg.tabId));
		case "show":
			return show(Number(msg.tabId));
		case "close":
			await ownTab(Number(msg.tabId));
			await chrome.tabs.remove(Number(msg.tabId));
			return { closed: true };
		default:
			throw new Error(`unknown command: ${msg.cmd}`);
	}
}

// ------------------------------------------------------------------ background tabs

/** A tab of our own, opened behind the one you're on, in the collapsed "Agent" group. */
async function backgroundTab(url) {
	const win = await chrome.windows.getLastFocused({ windowTypes: ["normal"] }).catch(() => null);
	if (!win) throw new Error("no browser window is open");
	const tab = await chrome.tabs.create({ url, active: false, windowId: win.id });
	try {
		const [group] = await chrome.tabGroups.query({ windowId: win.id, title: "Agent" });
		const groupId = await chrome.tabs.group(
			group ? { tabIds: tab.id, groupId: group.id } : { tabIds: tab.id, createProperties: { windowId: win.id } },
		);
		await chrome.tabGroups.update(groupId, { title: "Agent", color: "orange", collapsed: true });
	} catch {
		// Grouping is tidiness only.
	}
	return tab;
}

function loaded(tabId, timeout = 20000) {
	return new Promise((resolve) => {
		const timer = setTimeout(done, timeout); // read whatever is there by then
		function listener(id, info) {
			if (id === tabId && info.status === "complete") done();
		}
		function done() {
			clearTimeout(timer);
			chrome.tabs.onUpdated.removeListener(listener);
			resolve();
		}
		chrome.tabs.onUpdated.addListener(listener);
		// It may have finished before we started listening.
		chrome.tabs.get(tabId).then((t) => t.status === "complete" && t.url && t.url !== "about:blank" && done(), done);
	});
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Open, wait, run `fn` in the page, close. */
async function withTab(url, fn, args = []) {
	const tab = await backgroundTab(url);
	try {
		await loaded(tab.id);
		await sleep(700); // pages that render with JavaScript
		const current = await chrome.tabs.get(tab.id);
		if (isChallenge(current.url ?? "")) {
			throw new Error(
				"the site is showing a CAPTCHA / bot check; not attempting it. Try engine=duckduckgo, or open the page yourself.",
			);
		}
		const [{ result }] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: fn, args });
		return result;
	} finally {
		chrome.tabs.remove(tab.id).catch(() => {});
	}
}

function isChallenge(url) {
	return /google\.[a-z.]+\/sorry\//.test(url) || /duckduckgo\.com\/.*anomaly/.test(url) || /challenges\.cloudflare\.com/.test(url);
}

// ------------------------------------------------------------------ commands

async function search(q, engine, n) {
	if (!q.trim()) throw new Error("empty query");
	if (engine === "duckduckgo") {
		const results = await withTab(`https://html.duckduckgo.com/html/?q=${encodeURIComponent(q)}`, duckResults, [n]);
		return { engine, query: q, results };
	}
	const results = await withTab(`https://www.google.com/search?q=${encodeURIComponent(q)}&hl=en`, googleResults, [n]);
	return { engine: "google", query: q, results };
}

async function read(url, max) {
	if (!/^https?:\/\//.test(url)) throw new Error("only http(s) URLs can be read");
	return withTab(url, pageText, [max]);
}

async function extract(tabId, max) {
	const [{ result }] = await chrome.scripting.executeScript({ target: { tabId }, func: pageText, args: [max] });
	return result;
}

// ------------------------------------------------------------------ tabs the agent works in

async function agentTabs() {
	return (await chrome.storage.session.get("agentTabs")).agentTabs ?? [];
}

async function setAgentTabs(ids) {
	await chrome.storage.session.set({ agentTabs: ids });
}

chrome.tabs.onRemoved.addListener(async (tabId) => {
	const ids = await agentTabs();
	if (ids.includes(tabId)) await setAgentTabs(ids.filter((id) => id !== tabId));
});

async function ownTab(tabId) {
	if (!(await agentTabs()).includes(tabId)) {
		throw new Error(`tab ${tabId} is not one the agent opened: it can read any tab, but acts only in its own (open one first)`);
	}
}

async function open(url, showIt) {
	if (!/^https?:\/\//.test(url)) throw new Error("only http(s) URLs can be opened");
	const tab = await backgroundTab(url);
	await setAgentTabs([...(await agentTabs()), tab.id]);
	await loaded(tab.id);
	await sleep(700);
	if (showIt) await show(tab.id);
	return { tabId: tab.id, ...(await snapshot(tab.id, SNAPSHOT_TEXT)) };
}

async function snapshot(tabId, max) {
	const t = await chrome.tabs.get(tabId);
	const [{ result }] = await chrome.scripting.executeScript({ target: { tabId }, func: snapshotPage, args: [max] });
	if (isChallenge(t.url ?? "")) {
		result.challenge = "This page is a CAPTCHA / bot check. Don't attempt it: show the tab to the human.";
	}
	return result;
}

/** After an action: wait for any navigation it started to finish. */
async function settle(tabId, before) {
	await sleep(400);
	const t = await chrome.tabs.get(tabId);
	if (t.status === "loading" || t.url !== before) {
		await loaded(tabId);
		await sleep(500);
	}
}

async function act(tabId, msg) {
	await ownTab(tabId);
	const before = (await chrome.tabs.get(tabId)).url;
	const opts = { text: msg.text ?? "", value: msg.value ?? "", key: msg.key ?? "", dy: msg.dy ?? null, submit: !!msg.submit };
	const [{ result }] = await chrome.scripting.executeScript({
		target: { tabId },
		func: pageAction,
		args: [String(msg.action ?? ""), String(msg.ref ?? ""), opts, !!msg.confirmed],
	});
	if (result.error) throw new Error(result.error);
	if (result.needs_confirmation) return result;
	await settle(tabId, before);
	return { did: result.did, ...(await snapshot(tabId, msg.max ?? SNAPSHOT_TEXT)) };
}

async function navigate(tabId, url) {
	await ownTab(tabId);
	if (!/^https?:\/\//.test(url)) throw new Error("only http(s) URLs");
	await chrome.tabs.update(tabId, { url });
	await loaded(tabId);
	await sleep(700);
	return snapshot(tabId, SNAPSHOT_TEXT);
}

async function goBack(tabId) {
	await ownTab(tabId);
	const before = (await chrome.tabs.get(tabId)).url;
	await chrome.tabs.goBack(tabId);
	await settle(tabId, before);
	return snapshot(tabId, SNAPSHOT_TEXT);
}

/** Bring a tab to the human: out of the Agent group, active, its window raised (the bridge asks the compositor too). */
async function show(tabId) {
	const t = await chrome.tabs.get(tabId);
	if (t.groupId !== -1) await chrome.tabs.ungroup(tabId).catch(() => {});
	await chrome.tabs.update(tabId, { active: true });
	await chrome.windows.update(t.windowId, { focused: true }).catch(() => {});
	const now = await chrome.tabs.get(tabId);
	return { tabId, windowId: now.windowId, title: now.title, url: now.url, shown: true };
}

// ------------------------------------------------------------------ run inside pages (self-contained)

/** The page as an agent sees it: its text, and every visible control numbered (data-sketchpad-ref). */
function snapshotPage(max) {
	window.__sketchpadRef = window.__sketchpadRef || 0;
	const SEL =
		"a[href], button, input:not([type=hidden]), textarea, select, summary, [role=button], [role=link], [role=checkbox], " +
		"[role=radio], [role=tab], [role=menuitem], [role=option], [role=switch], [role=combobox], [role=textbox], " +
		"[contenteditable=''], [contenteditable=true]";
	const elements = [];
	for (const el of document.querySelectorAll(SEL)) {
		const r = el.getBoundingClientRect();
		if (r.width < 1 || r.height < 1) continue;
		const st = getComputedStyle(el);
		if (st.visibility === "hidden" || st.display === "none" || st.opacity === "0") continue;
		if (!el.dataset.sketchpadRef) el.dataset.sketchpadRef = `e${++window.__sketchpadRef}`;
		const tag = el.tagName.toLowerCase();
		const role = el.getAttribute("role") || (tag === "a" ? "link" : tag === "input" ? el.type || "text" : tag);
		const name = (
			el.getAttribute("aria-label") ||
			el.labels?.[0]?.innerText ||
			el.innerText ||
			el.placeholder ||
			el.title ||
			el.querySelector?.("img")?.alt ||
			el.name ||
			""
		)
			.replace(/\s+/g, " ")
			.trim()
			.slice(0, 80);
		const item = { ref: el.dataset.sketchpadRef, role, name };
		if ("value" in el && el.value && tag !== "button" && el.type !== "password" && !["checkbox", "radio", "submit"].includes(el.type)) {
			item.value = String(el.value).slice(0, 80);
		}
		if (el.type === "checkbox" || el.type === "radio") item.checked = el.checked;
		if (tag === "a" && el.href) item.href = el.href.slice(0, 120);
		if (el.disabled) item.disabled = true;
		if (r.bottom < 0 || r.top > innerHeight) item.offscreen = true;
		elements.push(item);
		if (elements.length >= 250) break;
	}
	const text = (document.body?.innerText ?? "").replace(/\n{3,}/g, "\n\n").trim();
	return {
		title: document.title,
		url: location.href,
		scroll: [Math.round(scrollY), document.documentElement.scrollHeight],
		elements,
		text: text.slice(0, max),
		truncated: text.length > max,
	};
}

/** One action on the page. Consequential ones come back as needs_confirmation unless confirmed. */
function pageAction(action, ref, o, confirmed) {
	const RISKY =
		/\b(buy|purchase|pay|order|checkout|check out|send|post|publish|tweet|reply|comment|delete|remove|confirm|submit|transfer|subscribe|unsubscribe|sign up|register|book|share|invite|approve|accept|donate)\b/i;
	const label = (el) =>
		(el.getAttribute("aria-label") || el.innerText || el.value || el.placeholder || el.title || el.name || "").replace(/\s+/g, " ").trim().slice(0, 80);
	const isSearch = (form) =>
		form.getAttribute("role") === "search" ||
		!!form.querySelector("input[type=search], input[name=q], input[name=query], input[name=search_query], input[name=s]") ||
		/search/i.test(form.getAttribute("action") || "");
	const needs = (what) => ({ needs_confirmation: true, action, what, site: location.hostname });
	const el = ref ? document.querySelector(`[data-sketchpad-ref="${CSS.escape(ref)}"]`) : null;
	if (ref && !el) return { error: `no element ${ref} on the page now: take a new snapshot` };
	const pressEnter = (target) => {
		const init = { key: "Enter", code: "Enter", keyCode: 13, which: 13, bubbles: true, cancelable: true };
		const go = target.dispatchEvent(new KeyboardEvent("keydown", init));
		target.dispatchEvent(new KeyboardEvent("keypress", init));
		target.dispatchEvent(new KeyboardEvent("keyup", init));
		return go;
	};
	switch (action) {
		case "click": {
			if (!el) return { error: "click needs a ref" };
			const form = el.form || el.closest("form");
			const submits = form && (el.type === "submit" || el.type === "image" || (el.tagName === "BUTTON" && !el.getAttribute("type")));
			const text = label(el);
			if (!confirmed && (RISKY.test(text) || (submits && !isSearch(form)))) return needs(`click "${text}"`);
			el.scrollIntoView({ block: "center" });
			for (const t of ["pointerdown", "mousedown", "pointerup", "mouseup"]) {
				const E = t.startsWith("pointer") ? PointerEvent : MouseEvent;
				el.dispatchEvent(new E(t, { bubbles: true, cancelable: true, view: window }));
			}
			el.click();
			return { did: `clicked "${text}"` };
		}
		case "type": {
			if (!el) return { error: "type needs a ref" };
			if (el.type === "password") return { error: "won't type into a password field: show the tab to the human so they sign in themselves" };
			const form = el.form || el.closest("form");
			const searchy = form ? isSearch(form) : /search/i.test(`${el.name} ${el.placeholder} ${el.getAttribute("aria-label")}`);
			if (o.submit && !confirmed && !searchy) return needs(`type "${o.text.slice(0, 60)}" and submit the form`);
			el.scrollIntoView({ block: "center" });
			el.focus();
			if (el.isContentEditable) {
				document.execCommand("selectAll");
				document.execCommand("insertText", false, o.text);
			} else {
				const proto = el.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
				Object.getOwnPropertyDescriptor(proto, "value").set.call(el, o.text);
				el.dispatchEvent(new Event("input", { bubbles: true }));
				el.dispatchEvent(new Event("change", { bubbles: true }));
			}
			if (o.submit && pressEnter(el) && form) form.requestSubmit();
			return { did: `typed "${o.text.slice(0, 60)}"${o.submit ? " and submitted" : ""}` };
		}
		case "select": {
			if (!el || el.tagName !== "SELECT") return { error: "select needs the ref of a <select>" };
			const opt = [...el.options].find((x) => x.value === o.value || x.text.trim() === o.value);
			if (!opt) return { error: `no option "${o.value}"` };
			el.value = opt.value;
			el.dispatchEvent(new Event("input", { bubbles: true }));
			el.dispatchEvent(new Event("change", { bubbles: true }));
			return { did: `selected "${opt.text.trim()}"` };
		}
		case "key": {
			const target = el || document.activeElement || document.body;
			const form = target.form || target.closest?.("form");
			if (o.key === "Enter" && form && !isSearch(form) && !confirmed) return needs("press Enter in a form (submits it)");
			if (el) el.focus();
			if (o.key === "Enter") {
				if (pressEnter(target) && form) form.requestSubmit();
			} else {
				const init = { key: o.key, bubbles: true, cancelable: true };
				target.dispatchEvent(new KeyboardEvent("keydown", init));
				target.dispatchEvent(new KeyboardEvent("keyup", init));
			}
			return { did: `pressed ${o.key}` };
		}
		case "scroll": {
			window.scrollBy(0, o.dy ?? innerHeight * 0.8);
			return { did: `scrolled to ${Math.round(scrollY)}` };
		}
		default:
			return { error: `unknown action: ${action}` };
	}
}



function pageText(max) {
	const root = document.querySelector("article") || document.querySelector("main") || document.body;
	const text = (root?.innerText ?? "").replace(/\n{3,}/g, "\n\n").trim();
	return { title: document.title, url: location.href, text: text.slice(0, max), truncated: text.length > max };
}

function googleResults(n) {
	const out = [];
	const seen = new Set();
	for (const h3 of document.querySelectorAll("a h3")) {
		const a = h3.closest("a");
		const url = a?.href;
		if (!url || seen.has(url) || !/^https?:/.test(url) || /google\.[a-z.]+\/(search|url)\?/.test(url)) continue;
		seen.add(url);
		// The result's block: climb until it holds the description too, then take its
		// longest line that isn't the title or the site / breadcrumb line.
		const title = h3.innerText.trim();
		let box = a;
		for (let up = 0; up < 8 && box.parentElement && box !== document.body; up++) {
			box = box.parentElement;
			if (box.innerText.length > title.length + 160) break;
		}
		const lines = box.innerText
			.split("\n")
			.map((l) => l.trim())
			.filter((l) => l && l !== title && !l.includes("›") && !/^https?:\/\//.test(l));
		const snippet = lines.sort((x, y) => y.length - x.length)[0] ?? "";
		out.push({ title, url, snippet: snippet.slice(0, 400) });
		if (out.length >= n) break;
	}
	return out;
}

function duckResults(n) {
	return [...document.querySelectorAll(".result")]
		.map((r) => {
			const a = r.querySelector(".result__a");
			let url = a?.href ?? "";
			const m = url.match(/[?&]uddg=([^&]+)/);
			if (m) url = decodeURIComponent(m[1]);
			return { title: a?.innerText.trim() ?? "", url, snippet: r.querySelector(".result__snippet")?.innerText.trim() ?? "" };
		})
		.filter((r) => r.url)
		.slice(0, n);
}
