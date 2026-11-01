//! Agent skill installer, modeled on Rex's `rex skill`:
//!
//! - A tiny *stub* SKILL.md goes into each agent's skills directory. It only
//!   carries the trigger description and says "run `<tool> __internal skill`".
//!   The real guide is compiled into this binary, so upgrading the tool
//!   upgrades what every agent knows, with no reinstall.
//! - A SessionStart *hook* (Claude Code, Codex) runs
//!   `<tool> __internal session-start`, which tells the agent in one line what
//!   is available right now, so it doesn't spend tokens finding out.
//! - Everything we write is marked, so `status` can say current / outdated /
//!   missing / foreign, `setup` is idempotent and never clobbers a skill it
//!   didn't write (unless --force), and `remove` takes out only what it put in.
//!
//! Settings files are merged, never replaced, and backed up before writing.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

pub struct Spec {
    pub name: &'static str,
    pub description: &'static str,
    pub guide: &'static str,
    /// One line of context for a new agent session, or None for silence.
    pub session_note: fn() -> Option<String>,
}

const STUB_VERSION: &str = "1";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Agent {
    Claude,
    Codex,
    OpenCode,
    Pi,
}

impl Agent {
    const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::OpenCode, Agent::Pi];

    fn key(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::OpenCode => "opencode",
            Agent::Pi => "pi",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::OpenCode => "OpenCode",
            Agent::Pi => "Pi",
        }
    }

    fn config_dir(self) -> PathBuf {
        let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
        match self {
            Agent::Claude => env("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude")),
            Agent::Codex => env("CODEX_HOME").unwrap_or_else(|| home().join(".codex")),
            Agent::OpenCode => home().join(".config/opencode"),
            Agent::Pi => env("PI_CODING_AGENT_DIR").unwrap_or_else(|| home().join(".pi/agent")),
        }
    }

    fn found(self) -> bool {
        self.config_dir().exists() || on_path(self.key()).is_some()
    }

    /// Where this agent reads user skills from.
    fn skill_dir(self, name: &str) -> PathBuf {
        match self {
            Agent::Claude => self.config_dir().join("skills").join(name),
            // Codex, OpenCode and Pi all read the shared Agent Skills location.
            _ => home().join(".agents/skills").join(name),
        }
    }

    fn hook_file(self) -> Option<PathBuf> {
        match self {
            Agent::Claude => Some(self.config_dir().join("settings.json")),
            Agent::Codex => Some(self.config_dir().join("hooks.json")),
            _ => None,
        }
    }

    /// Other places an earlier install of ours may have left a copy.
    fn legacy_dirs(self, name: &str) -> Vec<PathBuf> {
        match self {
            Agent::Pi => vec![self.config_dir().join("skills").join(name)],
            _ => Vec::new(),
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into())
}

fn on_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(program))
            .find(|p| p.is_file())
    })
}

fn tilde(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

fn marker(spec: &Spec) -> String {
    format!(
        "<!-- Installed by \"{0} skill setup\". Remove it with \"{0} skill remove\". -->",
        spec.name
    )
}

fn stub(spec: &Spec) -> String {
    format!(
        "---\nname: {name}\ndescription: {desc}\nmetadata:\n  {name}-stub: \"{STUB_VERSION}\"\n---\n{marker}\n\n\
         Run `{name} __internal skill` and follow the guide it prints. The guide comes from\n\
         the installed {name} binary, so it always matches the commands available.\n\n\
         If that command fails, the installed {name} predates the guide: use `{name} --help`.\n",
        name = spec.name,
        desc = spec.description,
        marker = marker(spec),
    )
}

/// The binary the hook should run: the one on PATH if it is this binary
/// (stable across rebuilds), else this executable's own path.
fn exe_for_hooks(spec: &Spec) -> String {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    match on_path(spec.name) {
        Some(p) if p.canonicalize().ok() == me => p.display().to_string(),
        _ => me
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| spec.name.to_owned()),
    }
}

fn hook_command(spec: &Spec, agent: Agent) -> String {
    format!(
        "/bin/sh -c '{} __internal session-start --agent {} 2>/dev/null || true'",
        exe_for_hooks(spec),
        agent.key()
    )
}

fn is_our_hook(spec: &Spec, command: &str) -> bool {
    command.contains(&format!("{} __internal session-start", spec.name))
}

// ------------------------------------------------------------------ state

#[derive(Debug, PartialEq, Eq)]
enum State {
    Current,
    Outdated,
    Missing,
    Foreign,
    Legacy,
    Unreadable,
}

impl State {
    fn word(&self) -> &'static str {
        match self {
            State::Current => "current",
            State::Outdated => "outdated",
            State::Missing => "missing",
            State::Foreign => "foreign",
            State::Legacy => "legacy",
            State::Unreadable => "unreadable",
        }
    }
}

fn skill_state(spec: &Spec, dir: &Path) -> State {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return State::Missing;
    };
    let file = dir.join("SKILL.md");
    let Ok(text) = fs::read_to_string(&file) else {
        return if meta.file_type().is_symlink() {
            State::Legacy
        } else {
            State::Unreadable
        };
    };
    if meta.file_type().is_symlink() {
        // An earlier hand install linked the repo's own skill directory here.
        let target = fs::read_link(dir).unwrap_or_default();
        let ours = target.ends_with(Path::new("skills").join(spec.name))
            && text.contains(&format!("name: {}", spec.name));
        return if ours { State::Legacy } else { State::Foreign };
    }
    if text == stub(spec) {
        State::Current
    } else if text.contains(&marker(spec)) {
        State::Outdated
    } else {
        State::Foreign
    }
}

fn read_json(path: &Path) -> Result<Option<Value>, String> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Some(json!({}))),
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{} is not valid JSON: {e}", tilde(path))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", tilde(path))),
    }
}

/// All our hook commands currently in a settings value.
fn our_hooks(spec: &Spec, settings: &Value) -> Vec<String> {
    settings
        .pointer("/hooks/SessionStart")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| g.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .filter(|c| is_our_hook(spec, c))
        .map(str::to_owned)
        .collect()
}

fn hook_state(spec: &Spec, agent: Agent, path: &Path) -> State {
    match read_json(path) {
        Err(_) => State::Unreadable,
        Ok(None) => State::Missing,
        Ok(Some(v)) => {
            let hooks = our_hooks(spec, &v);
            if hooks.is_empty() {
                State::Missing
            } else if hooks.len() == 1 && hooks[0] == hook_command(spec, agent) {
                State::Current
            } else {
                State::Outdated
            }
        }
    }
}

/// Remove our SessionStart entries (and any groups/keys they alone filled).
fn strip_hooks(spec: &Spec, settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(groups) = hooks.get_mut("SessionStart").and_then(Value::as_array_mut) {
        for g in groups.iter_mut() {
            if let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                list.retain(|h| {
                    !h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|c| is_our_hook(spec, c))
                });
            }
        }
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|l| !l.is_empty())
        });
        if groups.is_empty() {
            hooks.remove("SessionStart");
        }
    }
    if hooks.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
    }
}

fn write_json_with_backup(path: &Path, value: &Value) -> Result<(), String> {
    if path.exists() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let mut backup = PathBuf::from(format!("{}.bak-{stamp}", path.display()));
        let mut n = 2;
        while backup.exists() {
            backup = PathBuf::from(format!("{}.bak-{stamp}-{n}", path.display()));
            n += 1;
        }
        fs::copy(path, &backup).map_err(|e| format!("backup {}: {e}", tilde(path)))?;
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp-skill");
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())? + "\n";
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ commands

struct Opts {
    agents: Vec<Agent>,
    dry_run: bool,
    force: bool,
    no_hook: bool,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts {
        agents: Vec::new(),
        dry_run: false,
        force: false,
        no_hook: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" => opts.dry_run = true,
            "--force" => opts.force = true,
            "--no-hook" => opts.no_hook = true,
            "--agent" => {
                let name = it.next().ok_or("--agent needs a name")?;
                let agent = Agent::ALL
                    .into_iter()
                    .find(|ag| ag.key() == name)
                    .ok_or_else(|| format!("unknown agent {name} (claude, codex, opencode, pi)"))?;
                opts.agents.push(agent);
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    Ok(opts)
}

/// Agents to act on, grouped by the skill directory they share.
fn groups(agents: &[Agent], spec: &Spec) -> Vec<(String, PathBuf, Vec<Agent>)> {
    let mut out: Vec<(String, PathBuf, Vec<Agent>)> = Vec::new();
    for &a in agents {
        let dir = a.skill_dir(spec.name);
        match out.iter_mut().find(|g| g.1 == dir) {
            Some(g) => g.2.push(a),
            None => out.push((String::new(), dir, vec![a])),
        }
    }
    for g in &mut out {
        g.0 = g.2.iter().map(|a| a.title()).collect::<Vec<_>>().join(", ");
    }
    out
}

fn line(kind: &str, state: &str, path: &Path) {
    println!("  {kind:<6} {state:<12} {}", tilde(path));
}

pub fn run(spec: &Spec, args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("help");
    let opts = match parse(args.get(1..).unwrap_or(&[])) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    match sub {
        "show" => {
            print!("{}", spec.guide);
            0
        }
        "status" => status(spec, &opts),
        "setup" => setup(spec, &opts),
        "remove" => remove(spec, &opts),
        _ => {
            eprintln!(
                "{0} skill — teach coding agents to use {0}\n\n\
                 USAGE:\n  {0} skill setup  [--agent NAME]... [--dry-run] [--force] [--no-hook]\n  \
                 {0} skill status [--agent NAME]...\n  {0} skill remove [--agent NAME]... [--dry-run]\n  \
                 {0} skill show                 print the guide agents read\n\n\
                 Agents: claude (~/.claude/skills), codex, opencode, pi (~/.agents/skills).\n\
                 Hooks: Claude Code (settings.json) and Codex (hooks.json) get a SessionStart hook.",
                spec.name
            );
            2
        }
    }
}

fn selected(opts: &Opts) -> Vec<Agent> {
    if opts.agents.is_empty() {
        Agent::ALL.into_iter().filter(|a| a.found()).collect()
    } else {
        opts.agents.clone()
    }
}

fn status(spec: &Spec, opts: &Opts) -> i32 {
    let agents = if opts.agents.is_empty() {
        Agent::ALL.to_vec()
    } else {
        opts.agents.clone()
    };
    let mut needs_setup = false;
    for (title, dir, members) in groups(&agents, spec) {
        let found = members.iter().any(|a| a.found());
        println!("{title} ({})", if found { "found" } else { "not found" });
        let s = skill_state(spec, &dir);
        needs_setup |= found && s != State::Current;
        line("skill", s.word(), &dir);
        for a in &members {
            if let Some(f) = a.hook_file() {
                let h = hook_state(spec, *a, &f);
                needs_setup |= a.found() && h != State::Current;
                line("hook", h.word(), &f);
            }
            for legacy in a.legacy_dirs(spec.name) {
                let s = skill_state(spec, &legacy);
                if s != State::Missing {
                    needs_setup = true;
                    line("copy", s.word(), &legacy);
                }
            }
        }
        println!();
    }
    if needs_setup {
        println!(
            "Run \"{} skill setup\" to install or update the skill.",
            spec.name
        );
    }
    0
}

fn setup(spec: &Spec, opts: &Opts) -> i32 {
    let agents = selected(opts);
    if agents.is_empty() {
        println!("No agents found. Name one with --agent (claude, codex, opencode, pi).");
        return 1;
    }
    let verb = |done: &'static str, would: &'static str| if opts.dry_run { would } else { done };
    let mut failed = false;
    for (title, dir, members) in groups(&agents, spec) {
        println!("{title}");
        // The skill stub.
        let state = skill_state(spec, &dir);
        let action = match state {
            State::Current => "unchanged",
            State::Foreign if !opts.force => "skipped",
            State::Missing => verb("created", "would create"),
            _ => verb("updated", "would update"),
        };
        if !opts.dry_run && !matches!(action, "unchanged" | "skipped") {
            let result = (|| -> std::io::Result<()> {
                if let Ok(meta) = fs::symlink_metadata(&dir) {
                    if meta.file_type().is_symlink() || meta.is_file() {
                        fs::remove_file(&dir)?;
                    }
                }
                fs::create_dir_all(&dir)?;
                fs::write(dir.join("SKILL.md"), stub(spec))
            })();
            if let Err(e) = result {
                eprintln!("  skill  failed: {e}");
                failed = true;
            }
        }
        line("skill", action, &dir);
        if action == "skipped" {
            println!(
                "         (a skill named {} that we didn't install; --force replaces it)",
                spec.name
            );
        }

        for a in &members {
            // Copies an older install left where this agent also looks.
            for legacy in a.legacy_dirs(spec.name) {
                match skill_state(spec, &legacy) {
                    State::Legacy | State::Current | State::Outdated => {
                        if !opts.dry_run {
                            let _ =
                                fs::remove_file(&legacy).or_else(|_| fs::remove_dir_all(&legacy));
                        }
                        line("copy", verb("removed", "would remove"), &legacy);
                    }
                    _ => {}
                }
            }
            // The session-start hook.
            let Some(file) = a.hook_file() else { continue };
            if opts.no_hook {
                continue;
            }
            let state = hook_state(spec, *a, &file);
            if state == State::Unreadable {
                line("hook", "unreadable", &file);
                failed = true;
                continue;
            }
            if state == State::Current {
                line("hook", "unchanged", &file);
                continue;
            }
            let mut settings = read_json(&file).ok().flatten().unwrap_or_else(|| json!({}));
            strip_hooks(spec, &mut settings);
            if !settings.is_object() {
                line("hook", "unreadable", &file);
                failed = true;
                continue;
            }
            let entry = json!({"hooks": [{"type": "command", "command": hook_command(spec, *a), "timeout": 5}]});
            let hooks = settings
                .as_object_mut()
                .unwrap()
                .entry("hooks")
                .or_insert_with(|| json!({}));
            if !hooks.is_object() {
                line("hook", "unreadable", &file);
                failed = true;
                continue;
            }
            let groups = hooks
                .as_object_mut()
                .unwrap()
                .entry("SessionStart")
                .or_insert_with(|| json!([]));
            if let Some(list) = groups.as_array_mut() {
                list.push(entry);
            }
            if !opts.dry_run {
                if let Err(e) = write_json_with_backup(&file, &settings) {
                    eprintln!("  hook   failed: {e}");
                    failed = true;
                    continue;
                }
            }
            let action = if state == State::Missing {
                verb("created", "would create")
            } else {
                verb("updated", "would update")
            };
            line("hook", action, &file);
        }
        println!();
    }
    if opts.dry_run {
        println!("Dry run: nothing was written.");
    } else {
        println!("Start a new agent session to use it.");
        if agents.contains(&Agent::Codex) && !opts.no_hook {
            println!("Codex asks before running a new hook: approve it with /hooks.");
        }
    }
    i32::from(failed)
}

fn remove(spec: &Spec, opts: &Opts) -> i32 {
    let agents = if opts.agents.is_empty() {
        Agent::ALL.to_vec()
    } else {
        opts.agents.clone()
    };
    let verb = |done: &'static str, would: &'static str| if opts.dry_run { would } else { done };
    for (title, dir, members) in groups(&agents, spec) {
        println!("{title}");
        match skill_state(spec, &dir) {
            State::Current | State::Outdated | State::Legacy => {
                if !opts.dry_run {
                    let _ = fs::remove_file(&dir).or_else(|_| fs::remove_dir_all(&dir));
                }
                line("skill", verb("removed", "would remove"), &dir);
            }
            State::Foreign => line("skill", "kept", &dir),
            s => line("skill", s.word(), &dir),
        }
        for a in &members {
            for legacy in a.legacy_dirs(spec.name) {
                if matches!(
                    skill_state(spec, &legacy),
                    State::Legacy | State::Current | State::Outdated
                ) {
                    if !opts.dry_run {
                        let _ = fs::remove_file(&legacy).or_else(|_| fs::remove_dir_all(&legacy));
                    }
                    line("copy", verb("removed", "would remove"), &legacy);
                }
            }
            let Some(file) = a.hook_file() else { continue };
            match read_json(&file) {
                Ok(Some(mut v)) if !our_hooks(spec, &v).is_empty() => {
                    strip_hooks(spec, &mut v);
                    if !opts.dry_run {
                        if let Err(e) = write_json_with_backup(&file, &v) {
                            eprintln!("  hook   failed: {e}");
                            continue;
                        }
                    }
                    line("hook", verb("removed", "would remove"), &file);
                }
                Ok(_) => line("hook", "missing", &file),
                Err(_) => line("hook", "unreadable", &file),
            }
        }
        println!();
    }
    0
}

/// `<tool> __internal …` — what the stub and the hook call.
pub fn internal(spec: &Spec, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("skill") => {
            print!("{}", spec.guide);
            0
        }
        Some("session-start") => {
            // Never fail or block a session: print a note or nothing, exit 0.
            if let Some(note) = (spec.session_note)() {
                println!("{note}");
            }
            0
        }
        _ => 2,
    }
}
