//! Argument parsing (no dependency) and dispatch.

use crate::commands::{self, Ctx};
use crate::config::{self, Loaded};
use crate::git::Git;
use crate::steps::Mode;

pub const USAGE: &str = "edition: release lines, tags, channels and a deploy ledger

USAGE
  edition status                         lines, tags, what each channel holds
  edition plan <X.Y.Z>                   what a release needs; reads Concordance
  edition bump <version>                 set every version file (local)
  edition cut <X.Y>                      create release/X.Y from main
  edition rc <X.Y.Z>                     next pre-release tag on the line
  edition tag <X.Y.Z>                    final release tag on the line
  edition hotfix start <X.Y>             start a hotfix branch off release/X.Y
  edition hotfix finish <X.Y.Z>          tag the hotfix, then merge the line forward
  edition forward                        pull requests merging release lines into main
  edition deploy <channel> <ref>         put a ref on a channel (tier policy enforced)
  edition promote <from> <to>            deploy what one channel holds to another
  edition check-push <remote> [<url>]    content, version and pin checks for a push
  edition hooks install                  install the pre-push hook that runs check-push
  edition verify [--online]              check every invariant (exit 1 on error)
  edition ledger [--channel C]           print the ledger

OPTIONS
  -C, --config <path>   edition.toml (default: found in this directory or a parent)
  --root <dir>          the repository the config describes (default: the config's directory)
  --apply               run local steps (default: dry run that prints every command)
  --confirm <receipt>   run outward steps: the receipt the dry run of this exact plan printed
  --online              read the remote live for status, plan and verify (outward plans always do)
  --by <who>            recorded actor (default $EDITION_ACTOR, then git user.name)
  --build <label>       build label for app channels
  --approved-by <who>   approval for prod channels that need one
  --at <ref>            commit to cut or tag (default: the branch tip)
  --note <text>         note recorded in the ledger
  --channel <name>      ledger filter
  --allow-finding <id>  let one named content-check finding through (recorded)

ENVIRONMENT
  EDITION_ACTOR         recorded actor
  EDITION_EVENTS        append JSON-lines events here (`-` for standard error)
  EDITION_VAR_<NAME>    machine-specific values for {var.name}; printed by name, never recorded
  EDITION_RECEIPT_TTL   receipt lifetime in seconds (default 1800)

Tiers: dev (any ref, local, unrecorded), staging (tip of main only), prod (final release
tag on a release line, on the remote, every pin on a release tag).";

#[derive(Default)]
struct Opts {
    config: Option<String>,
    root: Option<String>,
    apply: bool,
    confirm: Option<String>,
    online: bool,
    by: Option<String>,
    build: Option<String>,
    approved_by: Option<String>,
    at: Option<String>,
    note: Option<String>,
    channel: Option<String>,
    allow_findings: Vec<String>,
    pos: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "-C" | "--config" => o.config = Some(val(a)?),
            "--root" => o.root = Some(val(a)?),
            "--apply" => o.apply = true,
            "--confirm" => o.confirm = Some(val(a)?),
            "--online" => o.online = true,
            "--by" => o.by = Some(val(a)?),
            "--build" => o.build = Some(val(a)?),
            "--approved-by" => o.approved_by = Some(val(a)?),
            "--at" => o.at = Some(val(a)?),
            "--note" => o.note = Some(val(a)?),
            "--channel" => o.channel = Some(val(a)?),
            "--allow-finding" => o.allow_findings.push(val(a)?),
            s if s.starts_with('-') && s != "-" => return Err(format!("unknown option {s}")),
            s => o.pos.push(s.to_string()),
        }
    }
    Ok(o)
}

pub fn run(args: &[String]) -> i32 {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return if args.is_empty() { 2 } else { 0 };
    }
    let o = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("edition: {e}\n\n{USAGE}");
            return 2;
        }
    };
    match dispatch(o) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("edition: {e}");
            2
        }
    }
}

fn need<'a>(pos: &'a [String], i: usize, what: &str) -> Result<&'a str, String> {
    pos.get(i).map(String::as_str).ok_or_else(|| format!("missing {what}"))
}

fn dispatch(o: Opts) -> Result<i32, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let path = config::find(o.config.as_deref(), &cwd)?;
    let mut l = Loaded::load(&path)?;
    if let Some(r) = &o.root {
        let dir = crate::util::resolve_path(&cwd, r);
        l.root = dir.canonicalize().map_err(|e| format!("--root {}: {e}", dir.display()))?;
    }
    let git = Git::new(&l.root);
    let by =
        o.by.clone()
            .or_else(|| std::env::var("EDITION_ACTOR").ok().filter(|s| !s.is_empty()))
            .or_else(|| git.user_name())
            .unwrap_or_else(|| "unknown".into());
    let ctx = Ctx {
        l,
        git,
        mode: Mode { apply: o.apply, confirm: o.confirm.clone() },
        by,
        online: o.online,
        build: o.build.clone(),
        approved_by: o.approved_by.clone(),
        at: o.at.clone(),
        note: o.note.clone(),
        allow_findings: o.allow_findings.clone(),
    };
    let p = &o.pos;
    let cmd = need(p, 0, "command")?;
    match cmd {
        "status" => commands::status(&ctx),
        "plan" => commands::plan(&ctx, need(p, 1, "version")?),
        "bump" => commands::bump(&ctx, need(p, 1, "version")?),
        "cut" => commands::cut(&ctx, need(p, 1, "line (X.Y)")?),
        "rc" => commands::rc(&ctx, need(p, 1, "version")?),
        "tag" => commands::tag(&ctx, need(p, 1, "version")?),
        "hotfix" => match need(p, 1, "start or finish")? {
            "start" => commands::hotfix_start(&ctx, need(p, 2, "line (X.Y)")?),
            "finish" => commands::hotfix_finish(&ctx, need(p, 2, "version")?),
            other => Err(format!("unknown hotfix step {other}")),
        },
        "forward" => commands::forward(&ctx),
        "deploy" => commands::deploy(&ctx, need(p, 1, "channel")?, need(p, 2, "ref")?),
        "promote" => commands::promote(&ctx, need(p, 1, "from channel")?, need(p, 2, "to channel")?),
        "hooks" => match need(p, 1, "install")? {
            "install" => commands::hooks_install(&ctx),
            other => Err(format!("unknown hooks step {other}")),
        },
        "check-push" => crate::content::check_push(&ctx.l, &ctx.git, need(p, 1, "remote")?),
        "verify" | "doctor" => {
            let r = crate::verify::run(&ctx.l, ctx.online);
            r.print();
            Ok(if r.errors.is_empty() { 0 } else { 1 })
        }
        "ledger" => commands::ledger_cmd(&ctx, o.channel.as_deref()),
        other => Err(format!("unknown command {other}\n\n{USAGE}")),
    }
}
